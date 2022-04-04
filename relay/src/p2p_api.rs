use async_trait::async_trait;
use futures_core::Stream;
use ethers::core::types::{TxHash, transaction::eip2718::TypedTransaction, Signature};
use tokio::sync::broadcast::{Sender, self, error::SendError};
use std::{error::Error, fmt::Debug, collections::{HashSet, hash_map::DefaultHasher}, hash::{Hash, Hasher}};
use tokio_stream::{StreamExt, wrappers::BroadcastStream};
use tracing::debug;

/// A trait for sending eth p2p messages to a peer
pub trait P2PSender {

}

/// Contains a typed transaction request and a signature
#[derive(Clone, Eq, Debug)]
pub struct SignedTx {
    pub tx: TypedTransaction,
    pub sig: Signature
}

// Hash implementation so it can be used in a HashSet
impl Hash for SignedTx {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write(&self.tx.rlp_signed(&self.sig).0[..]);
    }
}

impl PartialEq for SignedTx {
    fn eq(&self, other: &Self) -> bool {
        let mut hasher = DefaultHasher::new();
        self.hash(&mut hasher);
        let first_hash = hasher.finish();

        hasher = DefaultHasher::new();
        other.hash(&mut hasher);

        hasher.finish() == first_hash
    }
}

/// Provides a stream based interface for listening to pending transactions
#[async_trait]
pub trait MempoolListener: Sync + Send {
    type TxStream: Stream<Item = Result<SignedTx, Self::BroadcastError>> + Send + Unpin;
    type TxHashStream: Stream<Item = Result<TxHash, Self::BroadcastError>> + Send + Unpin;

    // TODO: make associated types nicer
    type BroadcastError: Sync + Send + Error;
    type Error: Sync + Send + Error;

    /// Subscribe to the incoming pending transactions
    fn subscribe_pending_txs(&self) -> Result<Self::TxStream, Self::Error>;

    /// Subscribe to the incoming pending transaction hashes
    fn subscribe_pending_hashes(&self) -> Result<Self::TxHashStream, Self::Error>;
}

/// Provides a deduplicating container for transactions
pub struct DedupStream<M> {
    pub sender: Sender<M>,

    /// Set that will be used to make sure we don't broadcast values we've already seen
    /// TODO: replace with a more space efficient alternative, maybe benchmark cuckoo filter.
    pub set: HashSet<M>,
}

impl<M> DedupStream<M> where
    M: Clone + Eq + Hash + Debug {
    /// Creates a new DedupStream with a capacity of 16384.
    pub fn new() -> Self {
        Self::new_with_capacity(16384)
    }

    /// Creates a new DedupStream with the given capacity.
    pub fn new_with_capacity(capacity: usize) -> Self {
        // basic capacity of 16384
        let sender = broadcast::channel(capacity).0;
        let set = HashSet::new();
        DedupStream { sender, set }
    }

    /// Insert into the stream, checking if the item exists.
    ///
    /// If the stream has not already broadcasted this value, true is returned.
    ///
    /// If the stream already broadcasted this value, false is returned.
    pub fn insert(&mut self, item: &M) -> Result<bool, SendError<M>> {
        if self.set.insert(item.clone()) {
            self.sender.send(item.clone()).map(|_| {Ok(true)})?
        } else {
            Ok(false)
        }
    }
}

impl<M> Default for DedupStream<M> where
    M: Clone + Eq + Hash + Debug {
    fn default() -> Self {
        DedupStream::new()
    }
}

#[tokio::test]
async fn test_proper_dedup() {
    let same_elem = 1;
    let next_elem = 2;
    let mut new_dedup = DedupStream::<usize>::new_with_capacity(4);
    let mut receiver = BroadcastStream::new(new_dedup.sender.subscribe());

    let first = new_dedup.insert(&same_elem).unwrap();
    assert!(first);
    let second = new_dedup.insert(&next_elem).unwrap();
    assert!(second);
    let third = new_dedup.insert(&next_elem).unwrap();
    assert!(!third);
    let fourth = new_dedup.insert(&same_elem).unwrap();
    assert!(!fourth);
    let result = receiver.next().await.unwrap().unwrap();
    assert_eq!(same_elem, result);

    // hopefully we have an error here rather than result
    let next_result = receiver.next().await.unwrap().unwrap();
    assert_eq!(next_elem, next_result);
}
