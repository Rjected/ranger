use crate::{
    p2p_api::{DedupStream, MempoolListener},
    PeerChains,
};
use akula::sentry::devp2p::{
    CapabilityName, CapabilityServer, CapabilityVersion, DisconnectReason, InboundEvent,
    Message as DevP2PMessage, OutboundEvent, PeerId,
};
use anvil_core::eth::{block::Block, transaction::TypedTransaction};
use arrayvec::{ArrayString, CapacityError};
use async_trait::async_trait;
use ethers::core::types::{ParseChainError, H512};
use ethp2p_rs::{
    EthMessage, EthMessageID, GetPooledTransactions, ProtocolMessage, RequestPair, Status,
};
use fastrlp::Encodable;
use foundry_config::Chain;
use parking_lot::RwLock;
use std::{
    collections::{HashMap, HashSet},
    convert::TryFrom,
    fmt::Debug,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
};
use thiserror::Error;
use tokio::sync::broadcast::{channel, error::SendError, Receiver, Sender};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::{debug, info};

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

    /// Current request id
    current_request_id: Arc<AtomicU32>,

    /// The status messages we hold for each chain
    pub status_map: Arc<RwLock<HashMap<u64, Status>>>,

    /// The set of peers we're connected to.
    valid_peers: Arc<RwLock<HashSet<H512>>>,

    /// Transactions that we've received
    new_transactions: Arc<RwLock<HashMap<[u8; 32], TypedTransaction>>>,

    /// Transactions that will be streamed
    hashes_stream: Arc<RwLock<DedupStream<[u8; 32]>>>,

    /// Transactions that will be streamed
    transaction_stream: Arc<RwLock<DedupStream<TypedTransaction>>>,

    /// Blocks available for broadcast
    // block_stream: Arc<RwLock<DedupStream<Block<Transaction>>>>,

    /// Two way map from peers to chains
    peer_chains: Arc<RwLock<PeerChains>>,

    /// Request info we use for relaying.
    requests_in_flight: Arc<RwLock<HashMap<InFlightRequest, RelayInfo>>>,

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

    /// Thrown if there is an issue converting a string into an ArrayString
    #[error("String error: {0}")]
    StringError(#[from] CapacityError<&'static str>),

    /// Thrown if there is an error inserting a transaction hash into the stream
    #[error("Error inserting transaction hash into stream: {0}")]
    TxHashSendError(#[from] SendError<[u8; 32]>),

    /// Thrown if there is an error inserting a transaction into the stream
    #[error("Error inserting transaction into stream: {0}")]
    TxSendError(#[from] SendError<TypedTransaction>),

    /// Thrown if we receive a BlockHeaders response but don't have a request in flight for it.
    #[error("Received block headers response from peer {peer_id:?} with request id {request_id:?} but there is no request in flight")]
    CannotFindRequest { request_id: u64, peer_id: PeerId },
}

impl P2PRelay {
    pub fn new() -> Self {
        debug!("P2PRelay started with debug");
        Self::default()
    }

    #[must_use]
    /// Adds the given status for use when sending handshakes to nodes.
    /// The input status will used during handshakes for the network contained in the status.
    pub fn with_status(self, status: Status) -> Self {
        {
            let mut status_map = self.status_map.write();
            status_map.insert(status.chain.id(), status);
        }

        // mark as ready to accept connections because we have at least one status message
        self.no_new_peers.store(false, Ordering::SeqCst);
        self
    }

    pub fn no_new_peers_handle(&self) -> Arc<AtomicBool> {
        self.no_new_peers.clone()
    }

    /// Set up the peer and add its open channels
    fn setup_peer(&self, peer: PeerId, pipe: Pipes) {
        let mut pipes = self.peer_pipes.write();

        pipes.entry(peer).or_insert(pipe);
    }

    /// Set up the peer's chain
    fn setup_peer_chain(&self, peer: PeerId, chain: Chain) {
        let mut peer_chains = self.peer_chains.write();
        peer_chains.add_peer(peer, chain);
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
        eth_message: EthMessage,
    ) -> Result<(), P2PRelayError> {
        let sender_pipe = self.sender(peer).ok_or(P2PRelayError::CannotFindPeer)?;

        // again, need more stable message types with encoding, decoding etc
        let mut encoded_message = vec![];
        eth_message.encode(&mut encoded_message);

        let prepared_message = OutboundEvent::Message {
            capability_name: CapabilityName(ArrayString::from("eth")?),
            message: DevP2PMessage {
                id: eth_message.message_id() as usize,
                data: encoded_message.into(),
            },
        };

        // TODO: figure out what an appropriate timeout would be - should we even use timeouts?
        sender_pipe
            .send(prepared_message)
            .map_err(P2PRelayError::SendTimeout)?;

        Ok(())
    }

    /// Relays a message to any peer we are currently connected to besides this peer!
    /// Returns the peer we sent the message to.
    /// TODO: want: a future on a message that is being sent to another peer (using a single peer
    /// OR multi peer sender). awaiting it should return Result<ExpectedResponseType, Error>.
    /// There are a bunch of messages in the eth protocol that you'd want responses from!
    /// Then we could basically do something like this:
    /// ```ignore
    /// async fn handle_message(&self, message: Message, peer: PeerId) {
    ///     // ensures we receive an answer to our query
    ///     let relay_response = self.send_eth_request(peer, message);
    ///
    /// }
    /// ```
    async fn relay_to_other_peer(
        &self,
        no_relay_to: PeerId,
        eth_message: EthMessage,
    ) -> Result<PeerId, P2PRelayError> {
        let next_peer = {
            let peers = self.peer_pipes.read();
            let peer_chains = self.peer_chains.read();
            let pair = peers
                .iter()
                .find(|kv| {
                    // TODO: how can we remove this unwrap? Maybe we should group pipes by chain
                    // directly rather than rely on pipes and chains being in sync?
                    let no_relay_chain = peer_chains.get_chain(&no_relay_to).unwrap();
                    kv.0 != &no_relay_to && peer_chains.is_on_chain(kv.0, no_relay_chain)
                })
                .ok_or(P2PRelayError::CannotFindRelayPeer)?;
            let next_peer = *pair.0;
            // explicitly drop peers because we don't need it and it shouldn't live any longer
            drop(peers);
            next_peer
        };

        self.send_to_peer(next_peer, eth_message).await?;
        Ok(next_peer)
    }

    /// Remove the specified peer from the list of valid peers
    fn teardown_peer(&self, peer: PeerId) {
        let mut valid_peers = self.valid_peers.write();
        let mut peer_chains = self.peer_chains.write();
        let peer_hash = H512::from_slice(peer.as_bytes());
        valid_peers.remove(&peer_hash);
        peer_chains.remove_peer(peer);
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
        message: EthMessage,
    ) -> Result<(), P2PRelayError> {
        match message {
            EthMessage::Status(their_status) => {
                debug!("Decoded status message from {}: {:X?}", peer, their_status);
                // TODO: I've seen some weird block messages from some other chains, it can be
                // diffcult to figure out the exact format, especially when the source may not be
                // available for these chains.
                // For now, let's only accept named chains, and figure out how to be multi-chain
                // without these types of issues
                if let Chain::Id(id) = their_status.chain {
                    self.teardown_peer(peer);
                    debug!("Status message from {} is for chain {}. Not connecting, here's the chain: {:X?}", peer, id, their_status);
                    return Ok(());
                }

                // Add the peer to the chain
                {
                    let mut peer_chains = self.peer_chains.write();
                    peer_chains.add_peer(peer, their_status.chain);
                }

                let our_status = {
                    let mut status_map = self.status_map.write();
                    // if we can't find the other peer's status, add it to the map so we can send
                    // it to peers later
                    match status_map.get(&u64::from(their_status.chain)) {
                        Some(&status) => status,
                        None => {
                            status_map.insert(u64::from(their_status.chain), their_status);
                            their_status
                        }
                    }
                };
                return self
                    .send_to_peer(peer, EthMessage::Status(our_status))
                    .await;
            }
            EthMessage::NewPooledTransactionHashes(new_pooled_transactions) => {
                // safely read from pooled_transactions
                let pooled_transactions_rc = Arc::new(new_pooled_transactions);

                // filter transactions out of the request that we've already seen
                let filtered_txids = {
                    let seen_txids = self.new_transactions.read();
                    pooled_transactions_rc
                        .0
                        .iter()
                        .filter_map(|seen| (!seen_txids.contains_key(seen)).then(|| *seen))
                        .collect()
                };

                // let's release the lock quickly
                {
                    let mut hashes_stream = self.hashes_stream.write();
                    for item in &pooled_transactions_rc.0 {
                        hashes_stream.insert(item)?;
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
                        EthMessage::GetPooledTransactions(RequestPair {
                            request_id: next_request_id as u64,
                            message: GetPooledTransactions(filtered_txids),
                        }),
                    )
                    .await;
            }
            EthMessage::PooledTransactions(RequestPair {
                request_id: _,
                message,
            }) => {
                let num_txs = message.0.len();
                info!("got {:?} pooled transactions from {}", num_txs, peer);
                {
                    let mut txs_stream = self.transaction_stream.write();
                    for item in message.0 {
                        txs_stream.insert(&item)?;
                    }
                }
            }
            EthMessage::Transactions(transactions) => {
                debug!("transactions from {}: {:?}", peer, transactions);
                {
                    let mut txs_stream = self.transaction_stream.write();
                    for item in transactions.0 {
                        txs_stream.insert(&item)?;
                    }
                }
            }
            EthMessage::GetBlockHeaders(get_block_headers) => {
                self.send_relay_message(peer, get_block_headers).await?;
            }
            EthMessage::BlockHeaders(block_headers) => {
                debug!(
                    "block headers from {:?}: {:?}... RELAYING",
                    peer, block_headers
                );
                self.receive_relay_message(peer, block_headers).await?;
            }
            EthMessage::GetBlockBodies(get_block_bodies) => {
                self.send_relay_message(peer, get_block_bodies).await?;
            }
            EthMessage::BlockBodies(block_bodies) => {
                self.receive_relay_message(peer, block_bodies).await?;
            }
            _ => {
                // debug!("other message received");
            }
        };
        Ok(())
    }

    /// Sends a request to another peer
    async fn send_relay_message<T>(
        &self,
        peer: PeerId,
        request: RequestPair<T>,
    ) -> Result<(), P2PRelayError>
    where
        RequestPair<T>: Into<EthMessage>,
    {
        // get a new request id
        let next_request_id = self.current_request_id.fetch_add(1, Ordering::SeqCst);

        // track the original request id
        let tracking_info = RelayInfo {
            peer_id: peer,
            original_id: request.request_id,
        };

        // create and send the new request
        let new_request = RequestPair {
            request_id: next_request_id as u64,
            message: request.message,
        };

        let new_peer = self.relay_to_other_peer(peer, new_request.into()).await?;
        let in_flight = InFlightRequest {
            new_request_id: next_request_id as u64,
            peer_id: new_peer,
        };

        // finally mark the request as in flight
        {
            let mut requests = self.requests_in_flight.write();
            requests.insert(in_flight, tracking_info);
        }
        Ok(())
    }

    /// Sends the given response to the peer that requested it
    async fn receive_relay_message<T>(
        &self,
        peer: PeerId,
        request: RequestPair<T>,
    ) -> Result<(), P2PRelayError>
    where
        T: Debug,
        RequestPair<T>: Into<EthMessage>,
    {
        // construct the in flight request info

        let request_key = InFlightRequest {
            new_request_id: request.request_id,
            peer_id: peer,
        };

        // get the original request id
        let relay_info = {
            let mut in_flight = self.requests_in_flight.write();
            in_flight
                .remove(&request_key)
                .ok_or(P2PRelayError::CannotFindRequest {
                    request_id: request.request_id,
                    peer_id: peer,
                })
        }?;

        // reconstruct the response
        let response = RequestPair {
            request_id: relay_info.original_id,
            message: request.message,
        };

        debug!(
            "received response from {:?}, relaying to {:?}: {:?}",
            peer, response, relay_info.peer_id
        );

        // send the response to the original peer
        self.send_to_peer(relay_info.peer_id, response.into()).await
    }
}

/// Represents a peer and a request id that we sent to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
struct InFlightRequest {
    /// The peer that we sent the relayed request to.
    pub peer_id: PeerId,
    /// The request id that we sent to the peer.
    pub new_request_id: u64,
}

/// Represents information needed to track a relayed request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
pub struct RelayInfo {
    /// The peer we are relaying messages to
    pub peer_id: PeerId,
    /// The request id that the peer sent us.
    pub original_id: u64,
}

impl Default for P2PRelay {
    fn default() -> Self {
        Self {
            peer_pipes: Default::default(),
            status_map: Default::default(),
            valid_peers: Default::default(),
            no_new_peers: Arc::new(AtomicBool::new(true)),
            transaction_stream: Default::default(),
            requests_in_flight: Default::default(),
            hashes_stream: Default::default(),
            peer_chains: Default::default(),
            new_transactions: Default::default(),
            current_request_id: Arc::new(AtomicU32::new(0)),
        }
    }
}

/// Allow for subscribing to new transactions and transaction hashes
#[async_trait]
impl MempoolListener for P2PRelay {
    type TxStream = BroadcastStream<TypedTransaction>;
    type TxHashStream = BroadcastStream<[u8; 32]>;
    type BlockStream = BroadcastStream<Block>;
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
        // TODO: send disconnect events if there is no status
        let (sender, receiver) = channel(1);
        self.setup_peer(
            peer,
            // we need to send the receiver here to the next() method
            Pipes { sender, receiver },
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
                // has an unwrap...
                let message_type = match EthMessageID::try_from(id) {
                    Ok(message_type) => message_type,
                    Err(error) => {
                        debug!("Invalid Eth message ID: {}! Kicking peer.", error);
                        self.disconnect_peer(peer).await;
                        // stop the program
                        return;
                    }
                };

                let protocol_message =
                    ProtocolMessage::decode_message(message_type, &mut &data[..]).unwrap_or_else(
                        |err| {
                            let hex_data = hex::encode(data.clone());
                            println!(
                                "rlp message with id {:?} failed to decode: {:?}",
                                id, hex_data
                            );
                            // hack to panic only if we fail to decode a message
                            panic!("Failed to decode message: {:?}", err);
                        },
                    );

                if let Err(reason) = self
                    .handle_eth_message(peer, protocol_message.message)
                    .await
                {
                    debug!("Error handling devp2p message: {}! Kicking peer.", reason);
                    self.disconnect_peer(peer).await;
                }
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

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use crate::P2PRelay;
    use akula::{p2p::node::PeerId, sentry::devp2p::OutboundEvent};
    use anvil_core::eth::block::Header;
    use ethereum_forkid::{ForkHash, ForkId};
    use ethp2p_rs::{
        BlockHashOrNumber, BlockHeaders, EthMessage, EthMessageID, EthVersion, GetBlockHeaders,
        ProtocolMessage, RequestPair, Status,
    };
    use foundry_config::Chain;
    use hex_literal::hex;
    use ruint::Uint;
    use tokio::sync::broadcast::channel;
    use tower::Service;

    use super::Pipes;

    #[tokio::test]
    async fn test_call_with_existing_status() {
        // this checks that we return an existing status if we have one
        let status = Status {
            version: EthVersion::Eth67 as u8,
            // ethers versions arent the same due to patches, so using Id here
            chain: Chain::Id(1),
            total_difficulty: Uint::from(36206751599115524359527u128),
            blockhash: hex!("feb27336ca7923f8fab3bd617fcb6e75841538f71c1bcfc267d7838489d9e13d"),
            genesis: hex!("d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3"),
            forkid: ForkId {
                hash: ForkHash([0xb7, 0x15, 0x07, 0x7d]),
                next: 0,
            },
        };

        // first add the status, then we will modify the status
        let mut relay = P2PRelay::new().with_status(status);

        let mut changed_status = status;
        changed_status.blockhash =
            hex!("d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3");
        // double unwrap because we expect a status to be returned
        let response = relay.call(changed_status.into()).await.unwrap().unwrap();

        assert_eq!(response, status.into());
    }

    #[tokio::test]
    async fn test_relaying_simple() {
        let status = Status {
            version: EthVersion::Eth67 as u8,
            // ethers versions arent the same due to patches, so using Id here
            chain: Chain::Id(1),
            total_difficulty: Uint::from(36206751599115524359527u128),
            blockhash: hex!("feb27336ca7923f8fab3bd617fcb6e75841538f71c1bcfc267d7838489d9e13d"),
            genesis: hex!("d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3"),
            forkid: ForkId {
                hash: ForkHash([0xb7, 0x15, 0x07, 0x7d]),
                next: 0,
            },
        };

        // first add the status, then we will modify the status
        let relay = P2PRelay::new().with_status(status);

        let first_peer_id = PeerId::random();
        let (first_sender, first_receiver) = channel(1);
        // need to subscribe so we can see the sent messages
        let mut first_peer_recv = first_sender.subscribe();
        let first_peer = Pipes {
            sender: first_sender,
            receiver: first_receiver,
        };
        relay.setup_peer(first_peer_id, first_peer);
        relay.setup_peer_chain(first_peer_id, status.chain);

        let second_peer_id = PeerId::random();
        let (second_sender, second_receiver) = channel(1);

        // need to subscribe so we can see the sent messages
        let mut second_peer_recv = second_sender.subscribe();
        let second_peer = Pipes {
            sender: second_sender,
            receiver: second_receiver,
        };
        relay.setup_peer(second_peer_id, second_peer);
        relay.setup_peer_chain(second_peer_id, status.chain);

        let simple_get_block_headers = RequestPair {
            request_id: 0,
            message: GetBlockHeaders {
                start_block: BlockHashOrNumber::Number(0),
                limit: 1,
                skip: 0,
                reverse: false,
            },
        };
        relay
            .handle_eth_message(first_peer_id, simple_get_block_headers.clone().into())
            .await
            .unwrap();

        // now we can check that the second peer got the message
        // unwrap because we expect a message to be returned
        let received_relay = match second_peer_recv.try_recv().unwrap() {
            OutboundEvent::Message { message, .. } => message,
            _ => panic!("Expected a message"),
        };

        // convert the message
        let message_type = EthMessageID::try_from(received_relay.id).unwrap();
        let received_message =
            ProtocolMessage::decode_message(message_type, &mut &received_relay.data[..]).unwrap();
        assert_eq!(received_message.message, simple_get_block_headers.into());

        let req_id = match received_message.message {
            EthMessage::GetBlockHeaders(RequestPair { request_id, .. }) => request_id,
            _ => panic!("Expected a GetBlockHeaders message"),
        };

        // now let's send a response
        let response: RequestPair<BlockHeaders> = RequestPair {
            request_id: req_id,
            message: vec![
                Header {
                    parent_hash: hex!("0000000000000000000000000000000000000000000000000000000000000000").into(),
                    ommers_hash: hex!("0000000000000000000000000000000000000000000000000000000000000000").into(),
                    beneficiary: hex!("0000000000000000000000000000000000000000").into(),
                    state_root: hex!("0000000000000000000000000000000000000000000000000000000000000000").into(),
                    transactions_root: hex!("0000000000000000000000000000000000000000000000000000000000000000").into(),
                    receipts_root: hex!("0000000000000000000000000000000000000000000000000000000000000000").into(),
                    logs_bloom: hex!("00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000").into(),
                    difficulty: 0x8aeu64.into(),
                    number: 0xd05u64.into(),
                    gas_limit: 0x115cu64.into(),
                    gas_used: 0x15b3u64.into(),
                    timestamp: 0x1a0au64,
                    extra_data: hex!("7788").into(),
                    mix_hash: hex!("0000000000000000000000000000000000000000000000000000000000000000").into(),
                    nonce: 0x0000000000000000u64.into(),
                    base_fee_per_gas: None,
                },
            ].into(),
        };

        // finally send the message
        relay
            .handle_eth_message(second_peer_id, response.clone().into())
            .await
            .unwrap();

        // now we can check that the first peer got the message
        // unwrap because we expect a message to be returned
        let relayed_message = match first_peer_recv.try_recv().unwrap() {
            OutboundEvent::Message { message, .. } => message,
            _ => panic!("Expected a message"),
        };

        // convert the message
        let message_type = EthMessageID::try_from(relayed_message.id).unwrap();
        let final_response =
            ProtocolMessage::decode_message(message_type, &mut &relayed_message.data[..]).unwrap();
        assert_eq!(final_response.message, response.into());
    }
}
