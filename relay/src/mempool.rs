use arrayvec::ArrayString;
use async_trait::async_trait;
use akula::sentry::devp2p::{
    CapabilityId, CapabilityName, CapabilityServer, CapabilityVersion, InboundEvent, OutboundEvent,
    PeerId, Swarm, SwarmBuilder,
};
use ethereum_forkid::{ForkHash, ForkId};
use ethers_core::types::{transaction::eip2718::TypedTransaction, Chain, H256, U256};
use rlp::Encodable;
use std::{collections::HashMap, fmt::Debug};
use thiserror::Error;

/// `P2PMempool` is a trait for fetching mempool transactions by interacting with the p2p protocol.
pub trait P2PMempool: Send + Sync {
    /// Send a message on the network that gets transactions from the transaction pool by hash.
    fn get_pooled_transactions(
        &self,
        tx_hashes: Vec<H256>,
    ) -> Result<Vec<TypedTransaction>, P2PMempoolError>;

    /// Get the new mempool transaction hashes that have been broadcasted on the network.
    /// TODO: need to figure out if this should be some sort of subscription api, since the
    /// NewPooledTransactionHashes message is announced every so often, and whatever type
    /// implements this trait will need to store a ton of hashes if it lives for a long time.
    fn new_pooled_transaction_hashes(&self) -> Result<Vec<H256>, P2PMempoolError>;

    // TODO: should we immediately send a GetPooledTransactions message after receiving a
    // NewPooledTransactionHashes? The same peer should have the matching transactions, minus those
    // which have been recently included in a block. These can be left out.
    // We could also send this message to all peers, although it's unclear what benefit this would
    // provide. Maybe another peer would be faster than the peer that issued the
    // NewPooledTransactionHashes method.
}

