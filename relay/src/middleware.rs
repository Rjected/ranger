use crate::{relay::P2PRelayError, P2PRelay};
use async_trait::async_trait;
use ethereum_types::H256;
use ethers::prelude::{
    Block, Bytes, FromErr, Middleware, PendingTransaction, PubsubClient, SubscriptionStream,
    Transaction,
};
use thiserror::Error;

#[derive(Error, Debug)]
/// Error thrown when the client interacts with a
pub enum P2PMiddlewareError<M: Middleware> {
    #[error("{0}")]
    /// Thrown when the internal call to the p2p client fails
    P2PError(P2PRelayError),

    #[error("{0}")]
    /// Thrown when an internal middleware errors
    MiddlewareError(M::Error),
}

impl<M: Middleware> FromErr<M::Error> for P2PMiddlewareError<M> {
    fn from(src: M::Error) -> P2PMiddlewareError<M> {
        P2PMiddlewareError::MiddlewareError(src)
    }
}

/// An ethers middleware for getting pending transactions from the p2p network.
#[derive(Debug, Clone)]
pub struct P2PMiddleware<M> {
    inner: M,
    relay: P2PRelay,
}

impl<M> P2PMiddleware<M>
where
    M: Middleware,
{
    /// Creates a new P2PMiddleware
    pub fn new(inner: M, relay: P2PRelay) -> Self {
        Self { inner, relay }
    }
}

#[async_trait]
impl<M> Middleware for P2PMiddleware<M>
where
    M: Middleware,
{
    type Error = P2PMiddlewareError<M>;
    type Provider = M::Provider;
    type Inner = M;

    fn inner(&self) -> &M {
        &self.inner
    }

    /// Send the raw RLP encoded transaction to the entire Ethereum network and returns the
    /// transaction's hash. This will consume gas from the account that signed the transaction.
    async fn send_raw_transaction<'a>(
        &'a self,
        tx: Bytes,
    ) -> Result<PendingTransaction<'a, Self::Provider>, Self::Error> {
        todo!("Need to implement p2p sending and determine how to provide PendingTransaction state machine");
    }

    /// Subscribes to a stream of untrusted pending transaction hashes
    async fn subscribe_pending_txs(
        &self,
    ) -> Result<SubscriptionStream<'_, Self::Provider, H256>, Self::Error>
    where
        Self::Provider: PubsubClient,
    {
        // is it worth converting to this? The return type is unfortunate since it's not a generic
        // stream. How would we generalize the return type in Provider?
        // use case for creating this middleware: drop-in upgrade for pending transactions
        // think about swapping out other mempool providers as well
        // It seems like SubscriptionStream is specific to a node jsonrpc connection powered by
        // the subscription stream's inner provider?
        // Maybe instead we can initialize the SubscriptionStream with a different rx?
        // The idea is we don't want to use the SubscriptionStream provider, instead
        // initializing with our own type of Stream to use as the NotificationStream
        todo!("figure out if we can be compatible with SubscriptionStream");
        // self.relay
        //     .subscribe_pending_hashes()
        //     .map_err(|e| P2PMiddlewareError::P2PError(e))
    }

    /// Subscribes to blocks by listening for them on the p2p network.
    async fn subscribe_blocks(
        &self,
    ) -> Result<SubscriptionStream<'_, Self::Provider, Block<H256>>, Self::Error>
    where
        Self::Provider: PubsubClient,
    {
        // another issue with this api is that we could also return Block<Transaction>, but that's
        // not a method on Middleware
        // regardless we can return the correct info
        todo!("listen to blocks");
    }

    /// Gets a transaction by hash via p2p
    async fn get_transaction<T: Send + Sync + Into<H256>>(
        &self,
        transaction_hash: T,
    ) -> Result<Option<Transaction>, Self::Error> {
        todo!("send a GetTransactions message");
        // motivation for multi-peer sender
        // how to implement fine grained peer policy
        // would we want a different policy for different types of requests?
    }
}
