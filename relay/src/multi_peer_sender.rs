/// This keeps track of multiple peers, and will queue messages to send to those peers.
/// This will send each queued message to the set of peers that are currently active until the
/// messages are explicitly un-queued.
///
/// This should ensure that we get a response for the given request.
/// We should decide on the following policies for sending requests:
///  - Sending requests with priority for certain peers (for example the peer which sent a
///  NewPooledTransactions request might have the transaction bodies as well)
///  - Sending a given request to all peers, or load balancing requests across peers
struct MultiPeerSender {
    /// the peers we've marked as timed out - should we attempt to reconnect?

    /// the messages that we're attempting to send - let's send them round-robin style
}
