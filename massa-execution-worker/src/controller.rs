// Copyright (c) 2022 MASSA LABS <info@massa.net>

//! This module implements an execution controller.
//! See `massa-execution-exports/controller_traits.rs` for functional details.

use crate::execution::ExecutionState;
use crate::request_queue::{RequestQueue, RequestWithResponseSender};
use massa_execution_exports::{
    ExecutionAddressInfo, ExecutionConfig, ExecutionController, ExecutionError, ExecutionManager,
    ExecutionOutput, ReadOnlyExecutionRequest,
};
use massa_models::api::EventFilter;
use massa_models::output_event::SCOutputEvent;
use massa_models::prehash::PreHashSet;
use massa_models::stats::ExecutionStats;
use massa_models::{address::Address, amount::Amount, operation::OperationId};
use massa_models::{block::BlockId, slot::Slot};
use massa_storage::Storage;
use parking_lot::{Condvar, Mutex, RwLock};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Display;
use std::sync::Arc;
use tracing::info;

/// structure used to communicate with execution thread
pub(crate) struct ExecutionInputData {
    /// set stop to true to stop the thread
    pub stop: bool,
    /// list of newly finalized blocks, indexed by slot
    pub finalized_blocks: HashMap<Slot, (BlockId, Storage)>,
    /// new blockclique (if there is a new one), blocks indexed by slot
    pub new_blockclique: Option<HashMap<Slot, (BlockId, Storage)>>,
    /// queue for read-only execution requests and response MPSCs to send back their outputs
    pub readonly_requests: RequestQueue<ReadOnlyExecutionRequest, ExecutionOutput>,
}

impl Display for ExecutionInputData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "stop={:?}, finalized={:?}, blockclique={:?}, readonly={:?}",
            self.stop,
            self.finalized_blocks
                .iter()
                .map(|(slot, (id, _))| (*slot, *id))
                .collect::<BTreeMap<Slot, BlockId>>(),
            self.new_blockclique.as_ref().map(|bq| bq
                .iter()
                .map(|(slot, (id, _))| (*slot, *id))
                .collect::<BTreeMap<Slot, BlockId>>()),
            self.readonly_requests
        )
    }
}

impl ExecutionInputData {
    /// Creates a new empty `ExecutionInputData`
    pub fn new(config: ExecutionConfig) -> Self {
        ExecutionInputData {
            stop: Default::default(),
            finalized_blocks: Default::default(),
            new_blockclique: Default::default(),
            readonly_requests: RequestQueue::new(config.max_final_events),
        }
    }

    /// Takes the current input data into a clone that is returned,
    /// and resets self.
    pub fn take(&mut self) -> Self {
        ExecutionInputData {
            stop: std::mem::take(&mut self.stop),
            finalized_blocks: std::mem::take(&mut self.finalized_blocks),
            new_blockclique: std::mem::take(&mut self.new_blockclique),
            readonly_requests: self.readonly_requests.take(),
        }
    }
}

#[derive(Clone)]
/// implementation of the execution controller
pub struct ExecutionControllerImpl {
    /// input data to process in the VM loop
    /// with a wake-up condition variable that needs to be triggered when the data changes
    pub(crate) input_data: Arc<(Condvar, Mutex<ExecutionInputData>)>,
    /// current execution state (see execution.rs for details)
    pub(crate) execution_state: Arc<RwLock<ExecutionState>>,
}

impl ExecutionController for ExecutionControllerImpl {
    /// called to signal changes on the current blockclique, also listing newly finalized blocks
    ///
    /// # arguments
    /// * `finalized_blocks`: list of newly finalized blocks to be appended to the input finalized blocks. Each Storage owns the block and its ops/endorsements/parents.
    /// * `blockclique`: new blockclique, replaces the current one in the input. Each Storage owns the block and its ops/endorsements/parents.
    fn update_blockclique_status(
        &self,
        finalized_blocks: HashMap<Slot, (BlockId, Storage)>,
        new_blockclique: HashMap<Slot, (BlockId, Storage)>,
    ) {
        // update input data
        let mut input_data = self.input_data.1.lock();
        input_data.new_blockclique = Some(new_blockclique); // replace blockclique
        input_data.finalized_blocks.extend(finalized_blocks); // append finalized blocks
        self.input_data.0.notify_one(); // wake up VM loop
    }

    /// Get the generated execution events, optionally filtered by:
    /// * start slot
    /// * end slot
    /// * emitter address
    /// * original caller address
    /// * operation id
    fn get_filtered_sc_output_event(&self, filter: EventFilter) -> Vec<SCOutputEvent> {
        self.execution_state
            .read()
            .get_filtered_sc_output_event(filter)
    }

    /// Get a copy of a single datastore entry with its final and active values
    ///
    /// # Return value
    /// * `Vec<(final_data_entry, active_data_entry)>`
    fn get_final_and_active_data_entry(
        &self,
        input: Vec<(Address, Vec<u8>)>,
    ) -> Vec<(Option<Vec<u8>>, Option<Vec<u8>>)> {
        let lock = self.execution_state.read();
        let mut result = Vec::with_capacity(input.len());
        for (addr, key) in input {
            result.push(lock.get_final_and_active_data_entry(&addr, &key));
        }
        result
    }

    /// Get the final and candidate values of sequential balances.
    ///
    /// # Return value
    /// * `(final_balance, candidate_balance)`
    fn get_final_and_candidate_sequential_balances(
        &self,
        addresses: &[Address],
    ) -> Vec<(Option<Amount>, Option<Amount>)> {
        let lock = self.execution_state.read();
        let mut result = Vec::with_capacity(addresses.len());
        for addr in addresses {
            result.push(lock.get_final_and_candidate_sequential_balance(addr));
        }
        result
    }

    /// Return the active rolls distribution for the given `cycle`
    fn get_cycle_active_rolls(&self, cycle: u64) -> BTreeMap<Address, u64> {
        self.execution_state.read().get_cycle_active_rolls(cycle)
    }

    /// Executes a read-only request
    /// Read-only requests do not modify consensus state
    fn execute_readonly_request(
        &self,
        req: ReadOnlyExecutionRequest,
    ) -> Result<ExecutionOutput, ExecutionError> {
        let resp_rx = {
            let mut input_data = self.input_data.1.lock();

            // if the read-only queue is already full, return an error
            if input_data.readonly_requests.is_full() {
                return Err(ExecutionError::ChannelError(
                    "too many queued readonly requests".into(),
                ));
            }

            // prepare the channel to send back the result of the read-only execution
            let (resp_tx, resp_rx) =
                std::sync::mpsc::channel::<Result<ExecutionOutput, ExecutionError>>();

            // append the request to the queue of input read-only requests
            input_data
                .readonly_requests
                .push(RequestWithResponseSender::new(req, resp_tx));

            // wake up the execution main loop
            self.input_data.0.notify_one();

            resp_rx
        };

        // Wait for the result of the execution
        match resp_rx.recv() {
            Ok(result) => result,
            Err(err) => Err(ExecutionError::ChannelError(format!(
                "readonly execution response channel readout failed: {}",
                err
            ))),
        }
    }

    /// List which operations inside the provided list were not executed
    fn unexecuted_ops_among(
        &self,
        ops: &PreHashSet<OperationId>,
        thread: u8,
    ) -> PreHashSet<OperationId> {
        self.execution_state
            .read()
            .unexecuted_ops_among(ops, thread)
    }

    /// Gets infos about a batch of addresses
    fn get_addresses_infos(&self, addresses: &[Address]) -> Vec<ExecutionAddressInfo> {
        let mut res = Vec::with_capacity(addresses.len());
        let exec_state = self.execution_state.read();
        for addr in addresses {
            let (final_datastore_keys, candidate_datastore_keys) =
                exec_state.get_final_and_candidate_datastore_keys(addr);
            let (final_parallel_balance, candidate_parallel_balance) =
                exec_state.get_final_and_candidate_parallel_balance(addr);
            let (final_sequential_balance, candidate_sequential_balance) =
                exec_state.get_final_and_candidate_sequential_balance(addr);
            let (final_roll_count, candidate_roll_count) =
                exec_state.get_final_and_candidate_rolls(addr);
            res.push(ExecutionAddressInfo {
                final_datastore_keys,
                candidate_datastore_keys,
                final_parallel_balance: final_parallel_balance.unwrap_or_default(),
                candidate_parallel_balance: candidate_parallel_balance.unwrap_or_default(),
                final_sequential_balance: final_sequential_balance.unwrap_or_default(),
                candidate_sequential_balance: candidate_sequential_balance.unwrap_or_default(),
                final_roll_count,
                candidate_roll_count,
                future_deferred_credits: exec_state.get_address_future_deferred_credits(addr),
                cycle_infos: exec_state.get_address_cycle_infos(addr),
            });
        }
        res
    }

    /// Get execution statistics
    fn get_stats(&self) -> ExecutionStats {
        self.execution_state.read().get_stats()
    }

    /// Returns a boxed clone of self.
    /// Allows cloning `Box<dyn ExecutionController>`,
    /// see `massa-execution-exports/controller_traits.rs`
    fn clone_box(&self) -> Box<dyn ExecutionController> {
        Box::new(self.clone())
    }
}

/// Execution manager
/// Allows stopping the execution worker
pub struct ExecutionManagerImpl {
    /// input data to process in the VM loop
    /// with a wake-up condition variable that needs to be triggered when the data changes
    pub(crate) input_data: Arc<(Condvar, Mutex<ExecutionInputData>)>,
    /// handle used to join the worker thread
    pub(crate) thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl ExecutionManager for ExecutionManagerImpl {
    /// stops the worker
    fn stop(&mut self) {
        info!("stopping Execution controller...");
        // notify the worker thread to stop
        {
            let mut input_wlock = self.input_data.1.lock();
            input_wlock.stop = true;
            self.input_data.0.notify_one();
        }
        // join the execution thread
        if let Some(join_handle) = self.thread_handle.take() {
            join_handle.join().expect("VM controller thread panicked");
        }
        info!("execution controller stopped");
    }
}
