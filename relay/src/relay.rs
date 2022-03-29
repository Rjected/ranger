use async_stream::stream;
use async_trait::async_trait;
use thiserror::Error;
use bytes::{Bytes, BytesMut};
use ethereum_types::{H256, U256, U64};
use akula::{
    sentry_connector::{messages::{Message, EthMessageId, StatusMessage, GetBlockBodiesMessage, GetPooledTransactionsMessage, NewPooledTransactionHashesMessage}, message_decoder::*},
    sentry::{
        devp2p::{CapabilityName, CapabilityVersion, InboundEvent, OutboundEvent, PeerId, CapabilityServer, DisconnectReason, Message as DevP2PMessage},
        eth::{EthProtocolVersion, capability_name},
    }, models::{MessageWithSignature, TxType},
};
use secp256k1::rand::random;
use ethers::core::{types::{Signature, H512, Chain, ParseChainError, transaction::{eip2718::TypedTransaction, eip2930::AccessListItem}, Transaction, TransactionRequest, Eip2930TransactionRequest, Eip1559TransactionRequest, NameOrAddress}};
use futures_core::stream::BoxStream;
use parking_lot::RwLock;
use std::{
    str::FromStr,
    time::Duration,
    collections::{HashMap, HashSet},
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering, AtomicU32},
        Arc,
    }, usize, convert::TryFrom,
};
use tokio::sync::{
    // broadcast::{channel as broadcast_channel, Sender as BroadcastSender},
    mpsc::{channel, Sender, error::SendTimeoutError},
    Mutex as AsyncMutex,
};
use fastrlp::Encodable as FastEncodable;
use tokio_stream::StreamExt;
use tracing::{debug, warn, info};

/// The channel for sending messages to a peer
type OutboundSender = Sender<OutboundEvent>;

/// The channel for receiving messages from a peer
type OutboundReceiver = Arc<AsyncMutex<BoxStream<'static, OutboundEvent>>>;

/// This represents an active connection with a peer, containing a sender and receiver which can be
/// used to interact with the peer.
#[derive(Clone)]
struct Pipes {
    sender: OutboundSender,
    receiver: OutboundReceiver,
}

// TODO: figure out why this exists and determine if it is needed, or if we might want to change
// this in the future.
pub const BUFFERING_FACTOR: usize = 5;

// TODO: separate almost everything out by chain
/// P2PRelay contains information that is necessary for connecting to the ethereum gossip protocol.
/// In order to make a connection, we need at least a status message and protocol version to send
/// to new peers.
pub struct P2PRelay {
    /// A map between a peer ID and the currently active connection with that peer.
    peer_pipes: Arc<RwLock<HashMap<PeerId, Pipes>>>,

    /// The status message to relay to peers during an eth handshake.
    status_message: Arc<RwLock<Option<StatusMessage>>>,

    /// The protocol version to send during the RLPx handshake, to be included in a `Hello`
    /// message.
    protocol_version: EthProtocolVersion,

    /// The highest difficulty status messages we can identify for a given chain
    status_map: Arc<RwLock<HashMap<Chain, StatusMessage>>>,

    /// Total work for each chain
    total_work_map: Arc<RwLock<HashMap<Chain, U256>>>,

    /// The set of peers we're connected to.
    valid_peers: Arc<RwLock<HashSet<H512>>>,

    /// This maps a peer to a list of transaction hashes that the peer has requested.
    /// The transaction body should be retrieved from this list of hashes, ideally from the peer
    /// that requested it.
    hashes_from_peer: Arc<RwLock<HashMap<PeerId, H256>>>,

    /// Transactions that we've received
    new_transactions: Arc<RwLock<HashMap<H256, TypedTransaction>>>,

    /// Requests that we've sent
    sent_requests: Arc<RwLock<HashMap<usize, Message>>>,

    /// Current request id
    current_request_id: Arc<AtomicU32>,

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
    //
    // =====
    // The below comment might be obsolete
    // =====
    // This broadcast channel will be consumed to send prepared RLP messages to peers.
    // Maybe turn this into some sort of struct where we separate each message into message type,
    // and construct BroadcastSenders for each message type.
    // Not sure what splitting up message by type really gets us, but it's possible we only need a
    // certain type of node to respond to certain types of messages.
    // data_sender: BroadcastSender<InboundMessage>,

    // this was used to report peer connect / disconnect status to a node, but the node doesn't
    // really need to know that, since we appear as a sole node to even trusted peers.
    // peers_status_sender: BroadcastSender<PeersReply>,

    // TODO: Cache certain types of responses so we can quickly reply to some messages that we've
    // already seen - may have a bad effect on the network, as a peer might send something invalid
    // that gets us kicked off, or worse allows our node to act as a repeater of bad data

    /// Whether or not we should connect to any new peers.
    no_new_peers: Arc<AtomicBool>,
}

#[derive(Error, Debug)]
/// TODO: this doc
pub enum P2PRelayError {
    /// Thrown if we can't identify a chain in a status message
    #[error("Failed to parse chain: {0}")]
    InvalidChain(#[from] ParseChainError),

    /// Thrown if we time out when sending a message to a peer
    #[error("Timeout when sending message to a peer: {0}")]
    SendTimeout(#[from] SendTimeoutError<OutboundEvent>),

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
    CannotFindPeer
}

impl P2PRelay {
    pub fn new(protocol_version: EthProtocolVersion) -> Self {
        debug!("P2PRelay started with debug");
        Self {
            peer_pipes: Default::default(),
            status_message: Default::default(),
            protocol_version,
            status_map: Default::default(),
            valid_peers: Default::default(),
            no_new_peers: Arc::new(AtomicBool::new(true)),
            total_work_map: Default::default(),
            hashes_from_peer: Default::default(),
            new_transactions: Default::default(),
            sent_requests: Default::default(),
            current_request_id: Arc::new(AtomicU32::new(0)),
        }
    }

    pub fn no_new_peers_handle(&self) -> Arc<AtomicBool> {
        self.no_new_peers.clone()
    }

    /// View the current status messages sent by peers
    pub fn view_status_map(&self) -> HashMap<Chain, StatusMessage> {
        (*self.status_map.read()).clone()
    }

    fn setup_peer(&self, peer: PeerId, p: Pipes) {
        let mut pipes = self.peer_pipes.write();

        pipes.entry(peer).or_insert(p);
    }

    fn get_pipes(&self, peer: PeerId) -> Option<Pipes> {
        self.peer_pipes.read().get(&peer).cloned()
    }

    /// Get the message sender channel for the given peer
    pub fn sender(&self, peer: PeerId) -> Option<OutboundSender> {
        self.peer_pipes
            .read()
            .get(&peer)
            .map(|pipes| pipes.sender.clone())
    }

    /// Get the message receiver channel for the given peer
    fn receiver(&self, peer: PeerId) -> Option<OutboundReceiver> {
        self.peer_pipes
            .read()
            .get(&peer)
            .map(|pipes| pipes.receiver.clone())
    }

    /// Send an eth message to a peer. This relies on the capability name that is currently active.
    pub async fn send_to_peer(&self, peer: PeerId, eth_message: Message) -> Result<(), P2PRelayError> {
        let sender_pipe = self.sender(peer).ok_or(P2PRelayError::CannotFindPeer)?;

        let mut message_data = BytesMut::new();
        eth_message.encode(&mut message_data);

        let prepared_message = OutboundEvent::Message {
            capability_name: capability_name(),
            message: DevP2PMessage {
                id: eth_message.eth_id() as usize,
                data: message_data.freeze(),
            },
        };

        // TODO: figure out what an appropriate timeout would be - should we even use timeouts?
        sender_pipe.send_timeout(prepared_message, Duration::from_millis(20))
            .await
            .map_err(P2PRelayError::SendTimeout)?;

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

    pub fn set_status(&self, message: StatusMessage) {
        *self.status_message.write() = Some(message);
        self.no_new_peers.store(false, Ordering::SeqCst);
    }

    /// Disconnects a peer with a ProtocolBreach
    async fn disconnect_peer(&self, peer: PeerId) {
        match self.sender(peer) {
            Some(sender) => {
                let _ = sender.send(OutboundEvent::Disconnect { reason: DisconnectReason::ProtocolBreach }).await;
            }
            None => {
                self.teardown_peer(peer);
            }
        }
    }

    /// Handles a devp2p message with a peer id and payload.
    async fn handle_eth_message(&self, peer: PeerId, message: Message) -> Result<(), P2PRelayError> {
        // This method just matches on a message type and processes it
        match message {
            Message::Status(status) => {
                // currently all this does is track the highest difficulty (as claimed by the
                // peer) status message for each chain.
                debug!("Decoded status message from {}: {:?}", peer, status);
                let status_difficulty = U256::from_big_endian(&status.total_difficulty.to_be_bytes());

                // If the status message has a higher total difficulty then we save it with other
                // status messages
                let chain = Chain::try_from(status.network_id)?;
                let mut total_work = self.total_work_map.write();
                match total_work.get(&chain) {
                    Some(total_difficulty) => {
                        if status_difficulty > *total_difficulty {
                            let mut status_map = self.status_map.write();
                            total_work.insert(chain, status_difficulty);
                            status_map.insert(chain, status);
                        }
                    }
                    None => {
                        let mut status_map = self.status_map.write();
                        total_work.insert(chain, status_difficulty);
                        status_map.insert(chain, status);
                    }
                }
            }
            Message::BlockBodies(block_bodies) => {
                debug!("Decoded block bodies message from {}: {:?}", peer, block_bodies);
            }
            Message::NewPooledTransactionHashes(new_pooled_transactions) => {
                let num_txs_to_show = 8;
                if new_pooled_transactions.0.len() >= num_txs_to_show {
                    let slice = new_pooled_transactions.0.split_at(num_txs_to_show).0;
                    debug!("{:?} NEW POOLED TX HASHES FROM {}! Here are {:?} of them: {:?}", new_pooled_transactions.0.len(), peer, num_txs_to_show, slice);
                } else {
                    debug!("{:?} NEW POOLED TX HASHES FROM {}! Here are all of them: {:?}", new_pooled_transactions.0.len(), peer, new_pooled_transactions.0);
                }

                // filter transactions that we've seen out of the request - we put this in its own
                // block so the lock isn't held across an await
                let filtered_txids: Vec<H256> = {
                    let seen_txids = self.new_transactions.read();
                    new_pooled_transactions.0.iter().filter_map(|seen| (!seen_txids.contains_key(seen)).then(|| *seen)).collect()
                };

                // TODO: how to ensure that we get a transaction body for every corresponding tx
                // hash eventually?
                // Individual pipes manage this for a single peer, but we want to do this for
                // multiple potential peers.
                // Pipes will ensure that a peer receives a message, but what if we disconnect from
                // a peer and never see them again?
                // What if a peer times out?
                // In that case, we should send the current set of unmatched hases (hashes without
                // bodies) to the next most suitable peer, while the peer that sent the request is
                // unavailable (potentially forever).
                // We could have some data structure that will try to send to a specified peer (or
                // can send to a set of peers, without a specific peer in mind) first, then use
                // some other default criteria for selecting other peers in case the specified peer
                // message send fails.
                // Maybe there is a multi-connection version of sender, and a multi-connection
                // version of receiver?
                // If not, one could be designed and implemented.
                let next_request_id = self.current_request_id.fetch_add(1, Ordering::SeqCst);
                debug!("Sending GetPooledTransactionsMessage with request id {} to peer {}", next_request_id, peer);
                return self.send_to_peer(peer, Message::GetPooledTransactions(GetPooledTransactionsMessage {
                    request_id: next_request_id as u64,
                    tx_hashes: filtered_txids,
                })).await
            }

            // from eth/66 docs:
            //
            // This is the response to GetPooledTransactions, returning the requested transactions
            // from the local pool. The items in the list are transactions in the format described
            // in the main Ethereum specification.
            //
            // The transactions must be in same order as in the request, but it is OK to skip transactions
            // which are not available. This way, if the response size limit is reached, requesters will know
            // which hashes to request again (everything starting from the last returned transaction) and which
            // to assume unavailable (all gaps before the last returned transaction).
            //
            // It is permissible to first announce a transaction via NewPooledTransactionHashes,
            // but then to refuse serving it via PooledTransactions. This situation can arise when
            // the transaction is included in a block (and removed from the pool) in between the
            // announcement and the request.
            //
            // A peer may respond with an empty list if none of the hashes match transactions in
            // its pool.
            Message::PooledTransactions(pooled_transactions) => {

                let num_txs = pooled_transactions.transactions.len();
                info!("got {:?} pooled transactions from {}", num_txs, peer);

                for transaction in pooled_transactions.transactions {
                    let (typed_tx, sig) = transaction_from_message(transaction.clone()).unwrap();
                    // TODO: set up stream, insert this into stream if it hasn't been streamed
                    // already. Is there an efficient deduplicating stream?
                    info!("🦀 NEW TRANSACTION with hash {:?}: {:?}", typed_tx.hash(&sig), typed_tx);
                }
            }
            Message::NewBlock(new_block) => {
                debug!("new block: {:?} from {}", new_block, peer);
            }
            Message::BlockHeaders(block_headers) => {
                debug!("{:?} new block headers from {}", block_headers.headers.len(), peer);
            }
            Message::NewBlockHashes(new_block_hashes) => {
                debug!("new block hashes from {}: {:?}", peer, new_block_hashes);
            }
            Message::Transactions(transactions) => {
                debug!("transactions from {}: {:?}", peer, transactions);
            }
            Message::NodeData(node_data) => {
                debug!("node data from {}: {:?}", peer, node_data);
            }
            Message::Receipts(receipts) => {
                debug!("receipts from {}: {:?}", peer, receipts);
            }
            Message::GetReceipts(get_receipts) => {
                debug!("get receipts from {}: {:?}", peer, get_receipts);
            }
            Message::GetPooledTransactions(get_pooled_transactions) => {
                debug!("get pooled transactions from {}: {:?}", peer, get_pooled_transactions);
            }
            Message::GetBlockBodies(get_block_bodies) => {
                debug!("get block bodes from {}: {:?}", peer, get_block_bodies);
            }
            Message::GetBlockHeaders(get_block_headers) => {
                debug!("get block headers from {}: {:?}", peer, get_block_headers);
            }
            Message::GetNodeData(get_node_data) => {
                debug!("get node data from {}: {:?}", peer, get_node_data);
            }

            // We may want to prevent peering with our own relay nodes. To accomplish
            // this, we could use a protocol like GRPC, but we would still need to have some form
            // of authorization, to make sure that only our clients can connect.
            //
            // This could be done by having a list of IPs which are already trusted, or we could
            // have a list of authorized public keys (or hashed pubkeys), and include signatures in
            // each command we send.
            //
            // If we authorize the peer that we are receiving additional (non-standard-eth)
            // messages from, we can reuse the message id space for other types of commands to
            // control the relay.
            //
            // We should throw a message parsing error if we get these types of messages from
            // untrusted IPs, or if the signature fails to validate.
        };
        Ok(())
    }
}

// Just converts the akula MessageWithSignature (with an included hash) to a TypedTransaction.
// MessageWithSignature contains enough information to recover the sender, so we need to do that
// and populate the TypedTransaction
fn transaction_from_message(transaction: MessageWithSignature) -> Result<(TypedTransaction, Signature), rlp::DecoderError> {
    // TODO: remove unwrap
    // we would need to modify the decode_signed to do sender recovery in order for this to work.
    // Instead, let's just do it here. I'm not sure if we would need to decode a signed transaction
    // elsewhere in eth applications, so it might not make sense for decode_signed to be p2p
    // specific, whereas decode would be more general. then again, I'm not sure that decode_signed
    // decodes any encoding other than our own signed encoding.
    let from = transaction.recover_sender().unwrap();
    let to = match transaction.message.action() {
        akula::models::TransactionAction::Create => None,
        akula::models::TransactionAction::Call(address) => Some(NameOrAddress::Address(address)),
    };

    // interesting that the MessageWithSignature uses H256 types, R is a coordinate not a hash
    let sig = Signature {
        r: U256::from(&transaction.r().0),
        s: U256::from(&transaction.s().0),
        v: transaction.v() as u64,
    };
    info!("prev signature: {:?}, new signature: {:?}", transaction.signature, sig);

    let ethers_chain_id = transaction.message
        .chain_id()
        .map(|akula_chain_id| U64::from(akula_chain_id.0));

    // convert from akula's tx type to ethers - it seems like there is a bit of duplicated work
    // here. Interesting that geth would probably accept gigantic nonces, but akula would not (due
    // to the u64)
    let tx_result = match transaction.message {
        akula::models::Message::Legacy { chain_id: _, nonce, gas_price, gas_limit, action: _, value, input } => {
            TypedTransaction::Legacy(TransactionRequest {
                from: Some(from),
                to,
                gas_price: Some(U256::from_big_endian(&gas_price.to_be_bytes())),
                gas: Some(U256::from(gas_limit)),
                value: Some(U256::from_big_endian(&value.to_be_bytes())),
                nonce: Some(U256::from(nonce)),
                chain_id: ethers_chain_id,
                data: Some(input.into()),
            })
        }
        akula::models::Message::EIP2930 { chain_id: _, nonce, gas_price, gas_limit, action: _, value, input, access_list } => {
            TypedTransaction::Eip2930(Eip2930TransactionRequest {
                tx: TransactionRequest {
                    from: Some(from),
                    to,
                    gas_price: Some(U256::from_big_endian(&gas_price.to_be_bytes())),
                    gas: Some(U256::from(gas_limit)),
                    value: Some(U256::from_big_endian(&value.to_be_bytes())),
                    nonce: Some(U256::from(nonce)),
                    chain_id: ethers_chain_id,
                    data: Some(input.into()),
                },
                access_list: access_list
                    .iter()
                    .map(|item| AccessListItem { address: item.address, storage_keys: item.slots.clone() })
                    .collect::<Vec<AccessListItem>>()
                    .into()
            })
        }
        akula::models::Message::EIP1559 { chain_id: _, nonce, max_priority_fee_per_gas, max_fee_per_gas, gas_limit, action: _, value, input, access_list } => {
            TypedTransaction::Eip1559(Eip1559TransactionRequest {
                from: Some(from),
                to,
                gas: Some(U256::from(gas_limit)),
                value: Some(U256::from_big_endian(&value.to_be_bytes())),
                nonce: Some(U256::from(nonce)),
                chain_id: ethers_chain_id,
                data: Some(input.into()),
                access_list: access_list
                    .iter()
                    .map(|item| AccessListItem { address: item.address, storage_keys: item.slots.clone() })
                    .collect::<Vec<AccessListItem>>()
                    .into(),
                max_fee_per_gas: Some(U256::from_big_endian(&max_fee_per_gas.to_be_bytes())),
                max_priority_fee_per_gas: Some(U256::from_big_endian(&max_priority_fee_per_gas.to_be_bytes())),
            })
        }
    };

    Ok((tx_result, sig))
}

#[async_trait]
impl CapabilityServer for P2PRelay {
    fn on_peer_connect(&self, peer: PeerId, caps: HashMap<CapabilityName, CapabilityVersion>) {
        let first_events = if let Some(status_message) = &*self.status_message.read()
        {
            let mut status_data = BytesMut::new();
            status_message.encode(&mut status_data);
            vec![OutboundEvent::Message {
                capability_name: capability_name(),
                message: DevP2PMessage {
                    id: EthMessageId::Status as usize,
                    data: status_data.freeze(),
                },
            }]
        } else {
            vec![OutboundEvent::Disconnect {
                reason: DisconnectReason::DisconnectRequested,
            }]
        };

        let (sender, mut receiver) = channel(1);
        self.setup_peer(
            peer,
            Pipes {
                sender,
                receiver: Arc::new(AsyncMutex::new(Box::pin(stream! {
                    for event in first_events {
                        yield event;
                    }

                    while let Some(event) = receiver.recv().await {
                        yield event;
                    }
                }))),
            },
        );
    }

    async fn on_peer_event(&self, peer: PeerId, event: InboundEvent) {
        match event {
            InboundEvent::Disconnect { reason } => {
                if let Some(DisconnectReason::UselessPeer) = reason {
                    info!("Peer {} disconnected because we are a useless peer", peer);
                }
                debug!("Peer {} disconnect (reason: {:?}), tearing down peer.", peer, reason);
                self.teardown_peer(peer);
            }
            InboundEvent::Message {
                message: DevP2PMessage { id, data },
                ..
            } => {
                let eth_message_id = match EthMessageId::try_from(id) {
                    Ok(eth_message_id) => eth_message_id,
                    Err(error) => {
                        debug!("Invalid Eth message ID: {}! Kicking peer.", error);
                        self.disconnect_peer(peer).await;
                        return
                    }
                };
                let eth_message = match decode_rlp_message(eth_message_id, &data) {
                    Ok(message_id) => {
                        message_id
                    },
                    Err(error) => {
                        debug!("Error decoding devp2p message: {}! Kicking peer.", error);
                        self.disconnect_peer(peer).await;
                        return
                    }
                };
                if let Err(reason) = self.handle_eth_message(peer, eth_message).await {
                    debug!("Error handling devp2p message: {}! Kicking peer.", reason);
                    self.disconnect_peer(peer).await;
                }
            }
        };
    }

    async fn next(&self, peer: PeerId) -> OutboundEvent {
        self.receiver(peer)
            .unwrap()
            .lock()
            .await
            .next()
            .await
            .unwrap_or(OutboundEvent::Disconnect {
                reason: DisconnectReason::DisconnectRequested,
            })
    }
}
