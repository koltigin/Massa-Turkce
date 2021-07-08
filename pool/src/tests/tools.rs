use crypto::hash::Hash;
use models::{
    Address, Operation, OperationContent, OperationType, SerializationContext, SerializeCompact,
};

use crate::PoolConfig;

pub fn example_pool_config() -> (PoolConfig, u8, u64) {
    let mut nodes = Vec::new();
    for _ in 0..2 {
        let private_key = crypto::generate_random_private_key();
        let public_key = crypto::derive_public_key(&private_key);
        nodes.push((public_key, private_key));
    }
    let thread_count: u8 = 2;
    let operation_validity_periods: u64 = 50;
    let max_block_size = 1024 * 1024;
    let max_operations_per_block = 1024;
    (
        PoolConfig {
            max_pool_size_per_thread: 100000,
            max_operation_future_validity_start_periods: 200,
        },
        thread_count,
        operation_validity_periods,
    )
}

pub fn get_transaction(
    expire_period: u64,
    fee: u64,
    context: &SerializationContext,
) -> (Operation, u8) {
    let sender_priv = crypto::generate_random_private_key();
    let sender_pub = crypto::derive_public_key(&sender_priv);

    let recv_priv = crypto::generate_random_private_key();
    let recv_pub = crypto::derive_public_key(&recv_priv);

    let op = OperationType::Transaction {
        recipient_address: Address::from_public_key(&recv_pub).unwrap(),
        amount: 0,
    };
    let content = OperationContent {
        fee,
        op,
        sender_public_key: sender_pub,
        expire_period,
    };
    let hash = Hash::hash(&content.to_bytes_compact(context).unwrap());
    let signature = crypto::sign(&hash, &sender_priv).unwrap();

    (
        Operation { content, signature },
        Address::from_public_key(&sender_pub).unwrap().get_thread(2),
    )
}
