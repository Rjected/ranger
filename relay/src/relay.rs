use akula::{
    p2p::types::{GetPooledTransactions, Message, Status},
    sentry::{
        devp2p::{
            CapabilityName, CapabilityServer, CapabilityVersion, DisconnectReason, InboundEvent,
            Message as DevP2PMessage, OutboundEvent, PeerId,
        },
        eth::EthProtocolVersion,
    },
};
use async_trait::async_trait;
use ethereum_types::{H256, U256};
use ethers::{
    core::types::{transaction::eip2718::TypedTransaction, Chain, ParseChainError, TxHash, H512},
    prelude::{Block, Transaction},
};
use parking_lot::RwLock;
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    usize,
};
use thiserror::Error;
// use tokio::sync::mpsc::{channel, error::SendTimeoutError, Receiver, Sender};
use tokio::sync::broadcast::{channel, error::SendError, Receiver, Sender};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::{debug, info};

use crate::p2p_api::{DedupStream, MempoolListener, SignedTx};

/// Contains the sender and receiver structs for a peer.
#[derive(Debug)]
pub struct Pipes {
    sender: Sender<OutboundEvent>,
    receiver: Receiver<OutboundEvent>,
}

/// P2PRelay contains information that is necessary for connecting to the ethereum gossip protocol.
/// In order to make a connection, we need at least a status message and protocol version to send
/// to new peers.
#[derive(Clone, Debug)]
pub struct P2PRelay {
    /// A map between a peer ID and the currently active connection with that peer.
    peer_pipes: Arc<RwLock<HashMap<PeerId, Pipes>>>,

    /// The protocol version to send during the RLPx handshake, to be included in a `Hello`
    /// message.
    protocol_version: EthProtocolVersion,

    /// Current request id
    current_request_id: Arc<AtomicU32>,

    /// The status messages we hold for each chain
    status_map: Arc<RwLock<HashMap<u64, Status>>>,

    /// Total work for each chain
    total_work_map: Arc<RwLock<HashMap<Chain, U256>>>,

    /// The set of peers we're connected to.
    valid_peers: Arc<RwLock<HashSet<H512>>>,

    /// Transactions that we've received
    new_transactions: Arc<RwLock<HashMap<H256, TypedTransaction>>>,

    /// Transactions that will be streamed
    hashes_stream: Arc<RwLock<DedupStream<TxHash>>>,

    /// Transactions that will be streamed
    transaction_stream: Arc<RwLock<DedupStream<SignedTx>>>,

    /// Blocks available for broadcast
    // block_stream: Arc<RwLock<DedupStream<Block<Transaction>>>>,

    // TODO: lots of things in this struct are to be replaced with a multi peer sender / message
    // manager. We will un-queue requests that correspond to responses that we receive
    //
    // We should also ask (and listen) for blocks, and block headers, so we can:
    //  - update the status message
    //  - remove txs that have been included in blocks from the mempool - maybe have a threshold in
    //  case the user wants to keep txs in case a block gets re-orged.
    //
    // Since this is a gigantic comment, I'll put another feature that we might want:
    //  - filter transactions we've sent! This relay could also be used for sending transactions
    //  over p2p, so if we send any, does the client really need to hear about it? Also, we could
    //  have explicit filters.
    //
    //  Also TODO: design the aggregator / deduplicator, and how the aggregator + other p2p relay
    //  nodes will interact with each other.
    /// Whether or not we should connect to any new peers.
    no_new_peers: Arc<AtomicBool>,
}

#[derive(Error, Debug)]
/// Errors that occur when handling or responding to ETH p2p messages
pub enum P2PRelayError {
    /// Thrown if we can't identify a chain in a status message
    #[error("Failed to parse chain: {0}")]
    InvalidChain(#[from] ParseChainError),

    /// Thrown if the chain does not have a corresponding status message
    #[error("Chain is not supported: {0}")]
    UnsupportedChain(&'static str),

    /// Thrown if we time out when sending a message to a peer
    #[error("Timeout when sending message to a peer: {0}")]
    SendTimeout(#[from] SendError<OutboundEvent>),

    /// Thrown if we get a response to a Get* message which includes a request ID we haven't
    /// included in any requests
    #[error("Request ID from response does not match any requests")]
    InvalidRequestId,

    /// Thrown if we get a response to a Get* message which doesn't match the type of request we
    /// sent
    #[error("Response type does not match the request type")]
    InvalidResponseType,

    /// Thrown if we are trying to send a message to a peer that has been removed or cannot be
    /// found
    #[error("Cannot find peer or peer has been removed")]
    CannotFindPeer,

    /// Thrown if there are no connected peers we can relay to.
    #[error("Cannot find peer that can relay messages")]
    CannotFindRelayPeer,
}

impl P2PRelay {
    pub fn new(protocol_version: EthProtocolVersion) -> Self {
        debug!("P2PRelay started with debug");
        Self {
            peer_pipes: Default::default(),
            protocol_version,
            status_map: Default::default(),
            valid_peers: Default::default(),
            no_new_peers: Arc::new(AtomicBool::new(true)),
            transaction_stream: Default::default(),
            hashes_stream: Default::default(),
            // block_stream: Default::default(),
            total_work_map: Default::default(),
            new_transactions: Default::default(),
            current_request_id: Arc::new(AtomicU32::new(0)),
        }
    }

    #[must_use]
    /// Adds the given status for use when sending handshakes to nodes.
    /// The input status will used during handshakes for the network contained in the status.
    pub fn with_status(&self, status: Status) -> Self {
        let this = self.clone();
        {
            let mut status_map = this.status_map.write();
            // status doesn't have a network id yet, can't do this anymore
            // status_map.insert(status, status);
        }

        // mark as ready to accept connections because we have at least one status message
        this.no_new_peers.store(false, Ordering::SeqCst);
        this
    }

    pub fn no_new_peers_handle(&self) -> Arc<AtomicBool> {
        self.no_new_peers.clone()
    }

    /// Set upt the peer and add its open channels
    fn setup_peer(&self, peer: PeerId, pipe: Pipes) {
        let mut pipes = self.peer_pipes.write();

        pipes.entry(peer).or_insert(pipe);
    }

    /// Get the message sender channel for the given peer
    pub fn sender(&self, peer: PeerId) -> Option<Sender<OutboundEvent>> {
        self.peer_pipes
            .read()
            .get(&peer)
            .map(|pipe| pipe.sender.clone())
    }

    /// Get a message receiver channel for the given peer
    pub fn receiver(&self, peer: PeerId) -> Option<Receiver<OutboundEvent>> {
        self.peer_pipes
            .read()
            .get(&peer)
            .map(|pipe| pipe.sender.subscribe())
    }

    /// Send an eth message to a peer. This relies on the capability name that is currently active.
    pub async fn send_to_peer(
        &self,
        peer: PeerId,
        eth_message: Message,
    ) -> Result<(), P2PRelayError> {
        let sender_pipe = self.sender(peer).ok_or(P2PRelayError::CannotFindPeer)?;

        // again, need more stable message types with encoding, decoding etc
        // let mut message_data = BytesMut::new();
        // eth_message.encode(&mut message_data);

        // let prepared_message = OutboundEvent::Message {
        //     capability_name: capability_name(),
        //     message: DevP2PMessage {
        //         id: eth_message.id() as usize,
        //         data: message_data.freeze(),
        //     },
        // };

        // TODO: figure out what an appropriate timeout would be - should we even use timeouts?
        // sender_pipe
        //     .send_timeout(prepared_message, Duration::from_millis(20))
        //     .await
        //     .map_err(P2PRelayError::SendTimeout)?;

        Ok(())
    }

    /// Relays a message to any peer we are currently connected to besides this peer!
    /// TODO: want: a future on a message that is being sent to another peer (using a single peer
    /// OR multi peer sender). awaiting it should return Result<ExpectedResponseType, Error>.
    /// There are a bunch of messages in the eth protocol that you'd want responses from!
    /// Then we could basically do something like this:
    /// ```
    /// async fn handle_message(&self, message: Message, peer: PeerId) {
    ///     // ensures we receive an answer to our query
    ///     let relay_response = self.send_eth_request(peer, message);
    ///
    /// }
    /// ```
    async fn relay_to_other_peer(
        &self,
        no_relay_to: PeerId,
        eth_message: Message,
    ) -> Result<(), P2PRelayError> {
        let all_peers = self.peer_pipes.read();
        let (_next_peer, sender_pipe) = all_peers
            .iter()
            .find(|kv| kv.0 == &no_relay_to)
            .ok_or(P2PRelayError::CannotFindRelayPeer)?;

        // again, need more stable message types with encoding, decoding etc
        // let mut message_data = BytesMut::new();
        // eth_message.encode(&mut message_data);

        // let prepared_message = OutboundEvent::Message {
        //     capability_name: capability_name(),
        //     message: DevP2PMessage {
        //         id: eth_message.id() as usize,
        //         data: message_data.freeze(),
        //     },
        // };

        // TODO: figure out what an appropriate timeout would be - should we even use timeouts?
        // sender_pipe
        //     .sender
        //     .send_timeout(prepared_message, Duration::from_millis(20))
        //     .await
        //     .map_err(P2PRelayError::SendTimeout)?;

        Ok(())
    }

    /// Remove the specified peer from the list of valid peers
    fn teardown_peer(&self, peer: PeerId) {
        let mut valid_peers = self.valid_peers.write();
        let peer_hash = H512::from_slice(peer.as_bytes());
        valid_peers.remove(&peer_hash);
    }

    pub fn all_peers(&self) -> HashSet<PeerId> {
        self.peer_pipes.read().keys().copied().collect()
    }

    pub fn connected_peers(&self) -> usize {
        self.valid_peers.read().len()
    }

    /// Disconnects a peer with a ProtocolBreach
    async fn disconnect_peer(&self, peer: PeerId) {
        match self.sender(peer) {
            Some(sender) => {
                let _ = sender.send(OutboundEvent::Disconnect {
                    reason: DisconnectReason::ProtocolBreach,
                });
            }
            None => {
                self.teardown_peer(peer);
            }
        }
    }

    /// Handles a devp2p message with a peer id and payload.
    async fn handle_eth_message(
        &self,
        peer: PeerId,
        message: Message,
    ) -> Result<(), P2PRelayError> {
        // This method just matches on a message type and processes it
        match message {
            // need more stable message enum
            // Message::Status(their_status) => {
            //     debug!("Decoded status message from {}: {:?}", peer, their_status);
            //     let our_status = {
            //         let status_map = self.status_map.read();
            //         // if we can't parse the network let's just not handle the message for now
            //         status_map
            //             .get(&their_status.network_id)
            //             .ok_or(P2PRelayError::UnsupportedChain("Unsupported chain!!!!"))?
            //             .clone()
            //     };
            //     return self.send_to_peer(peer, Message::Status(our_status)).await;
            // }
            Message::NewPooledTransactionHashes(new_pooled_transactions) => {
                let num_txs_to_show = 8;
                if new_pooled_transactions.0.len() >= num_txs_to_show {
                    let slice = new_pooled_transactions.0.split_at(num_txs_to_show).0;
                    debug!(
                        "{:?} NEW POOLED TX HASHES FROM {}! Here are {:?} of them: {:?}",
                        new_pooled_transactions.0.len(),
                        peer,
                        num_txs_to_show,
                        slice
                    );
                } else {
                    debug!(
                        "{:?} NEW POOLED TX HASHES FROM {}! Here are all of them: {:?}",
                        new_pooled_transactions.0.len(),
                        peer,
                        new_pooled_transactions.0
                    );
                }

                // filter transactions that we've seen out of the request - we put this in its own
                // block so the lock isn't held across an await
                let filtered_txids: Vec<H256> = {
                    let seen_txids = self.new_transactions.read();
                    new_pooled_transactions
                        .0
                        .iter()
                        .filter_map(|seen| (!seen_txids.contains_key(seen)).then(|| *seen))
                        .collect()
                };

                // let's release the lock quickly
                {
                    let mut hashes_stream = self.hashes_stream.write();
                    for item in filtered_txids.clone() {
                        // TODO: remove unwrap
                        hashes_stream.insert(&item).unwrap();
                    }
                }

                let next_request_id = self.current_request_id.fetch_add(1, Ordering::SeqCst);
                debug!(
                    "Sending GetPooledTransactionsMessage with request id {} to peer {}",
                    next_request_id, peer
                );
                return self
                    .send_to_peer(
                        peer,
                        Message::GetPooledTransactions(GetPooledTransactions {
                            request_id: next_request_id as u64,
                            hashes: filtered_txids,
                        }),
                    )
                    .await;
            }

            Message::PooledTransactions(pooled_transactions) => {
                let num_txs = pooled_transactions.transactions.len();
                info!("got {:?} pooled transactions from {}", num_txs, peer);

                // TODO: need more stable conversions
                // for transaction in pooled_transactions.transactions {
                //     let (typed_tx, sig) = transaction_from_message(transaction.clone()).unwrap();
                //     let mut tx_stream = self.transaction_stream.write();
                //     // TODO: remove unwrap
                //     tx_stream.insert(&SignedTx { tx: typed_tx, sig }).unwrap();
                // }
            }
            Message::NewBlock(new_block) => {
                debug!("new block: {:?} from {}", new_block, peer);
                let block = new_block.block;
                {
                    // let block_stream = self.block_stream.write();
                    // block_stream.insert(block);
                }
            }
            Message::BlockHeaders(block_headers) => {
                debug!(
                    "{:?} new block headers from {}",
                    block_headers.headers.len(),
                    peer
                );
            }
            Message::NewBlockHashes(new_block_hashes) => {
                debug!("new block hashes from {}: {:?}", peer, new_block_hashes);
            }
            Message::Transactions(transactions) => {
                debug!("transactions from {}: {:?}", peer, transactions);
            }
            Message::GetPooledTransactions(get_pooled_transactions) => {
                debug!(
                    "get pooled transactions from {}: {:?}",
                    peer, get_pooled_transactions
                );
            }
            Message::GetBlockBodies(get_block_bodies) => {
                info!("get block bodes from {}: {:?}", peer, get_block_bodies);
            }
            Message::GetBlockHeaders(get_block_headers) => {
                info!("get block headers from {}: {:?}", peer, get_block_headers);
                // send this to any peer!
            }
            _ => {
                // debug!("other message received");
            }
        };
        Ok(())
    }
}

/// Allow for subscribing to new transactions and transaction hashes
#[async_trait]
impl MempoolListener for P2PRelay {
    type TxStream = BroadcastStream<SignedTx>;
    type TxHashStream = BroadcastStream<TxHash>;
    type BlockStream = BroadcastStream<Block<Transaction>>;
    type Error = P2PRelayError;
    type BroadcastError = BroadcastStreamRecvError;

    fn subscribe_pending_txs(&self) -> Result<Self::TxStream, Self::Error> {
        let txs = self.transaction_stream.read();
        Ok(BroadcastStream::new(txs.sender.subscribe()))
    }

    fn subscribe_pending_hashes(&self) -> Result<Self::TxHashStream, Self::Error> {
        let hashes = self.hashes_stream.read();
        Ok(BroadcastStream::new(hashes.sender.subscribe()))
    }

    fn subscribe_blocks(&self) -> Result<Self::BlockStream, Self::Error> {
        // let blocks = self.blocks;
        todo!()
    }
}

#[async_trait]
impl CapabilityServer for P2PRelay {
    fn on_peer_connect(&self, peer: PeerId, _caps: HashMap<CapabilityName, CapabilityVersion>) {
        let disconnect_event =
            self.status_map
                .read()
                .is_empty()
                .then(|| OutboundEvent::Disconnect {
                    reason: DisconnectReason::DisconnectRequested,
                });

        let (sender, mut receiver) = channel(1);
        self.setup_peer(
            peer,
            // we need to send the receiver here to the next() method
            Pipes { sender, receiver }, // Pipes {
                                        //     sender,
                                        //     receiver: Arc::new(AsyncMutex::new(Box::pin(stream! {
                                        //         if let Some(event) = disconnect_event {
                                        //             yield event;
                                        //         }

                                        //         while let Some(event) = receiver.recv().await {
                                        //             yield event;
                                        //         }
                                        //     }))),
                                        // },
        );
    }

    async fn on_peer_event(&self, peer: PeerId, event: InboundEvent) {
        match event {
            InboundEvent::Disconnect { reason } => {
                if let Some(DisconnectReason::UselessPeer) = reason {
                    info!("Peer {} disconnected because we are a useless peer", peer);
                }
                info!(
                    "Peer {} disconnect (reason: {:?}), tearing down peer.",
                    peer, reason
                );
                self.teardown_peer(peer);
            }
            InboundEvent::Message {
                message: DevP2PMessage { id, data },
                ..
            } => {
                // id from int does not exist any more
                // let eth_message_id = match EthMessageId::try_from(id) {
                //     Ok(eth_message_id) => eth_message_id,
                //     Err(error) => {
                //         debug!("Invalid Eth message ID: {}! Kicking peer.", error);
                //         self.disconnect_peer(peer).await;
                //         return;
                //     }
                // };
                let hex_data = hex::encode(data);
                println!("{:?}", hex_data);

                // can't do this any more either - status changed
                // self.send_to_peer(peer, Message::Status(our_status)).await;
                // let eth_message = match decode_rlp_message(eth_message_id, &data) {
                //     Ok(message_id) => message_id,
                //     Err(error) => {
                //         debug!("Error decoding devp2p message: {}! Kicking peer.", error);
                //         self.disconnect_peer(peer).await;
                //         return;
                //     }
                // };
                // if let Err(reason) = self.handle_eth_message(peer, eth_message).await {
                //     debug!("Error handling devp2p message: {}! Kicking peer.", reason);
                //     self.disconnect_peer(peer).await;
                // }
            }
        };
    }

    async fn next(&self, peer: PeerId) -> OutboundEvent {
        self.receiver(peer)
            .unwrap()
            .recv()
            .await
            .unwrap_or(OutboundEvent::Disconnect {
                reason: DisconnectReason::DisconnectRequested,
            })
    }
}
