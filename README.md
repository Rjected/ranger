# Ranger

This listens to the [peer to peer network](https://github.com/ethereum/devp2p/blob/master/caps/eth.md) without a node, serving mempool messages and other data from p2p protocols.

# TODO: Revise below this line!

## High level functionality
This needs to listen for the following messages:
 - `NewPooledTransactionHashes`
 - `PooledTransactions`
 - `Transactions`

And send the following messages:
 - `GetPooledTransactions`

The `GetPooledTransactions` messages will be populated with what we receive from `NewPooledTransactionHashes` messages.
Using the response to `GetPooledTransactions`, we can serve these decoded transactions, or alternatively just their hashes.
 - [x] Connect to nodes on the p2p network
   - [x] Design peer discovery and management
     - Sentry will handle this
 - Need to make sure we understand whether or not a peer has omitted transactions due to a response limit, and implement re-sending a `GetPooledTransaction` message.
 - [ ] Implement method for automatically updating devp2p [`Status`](https://github.com/ethereum/devp2p/blob/master/caps/eth.md#status-0x00) messages using a trusted node. Currently these are hardcoded.
   - [ ] Test with other chains
 - [ ] Design easy to use API / stream aggregator for mempool transactions
   - Should be compatible with multiple remote p2p listening nodes
   - Should deduplicate transactions received from listening nodes
   - Decide whether or not sanity checks should be performed by default

### Other features to consider
   - [ ] Provide an API without any validation, as well as one with validation.

### Questions to answer
 - How should we respond to `NewPooledTransationHashes`?
 - Should we try to keep an updated transaction hash list on each sniffer? How would we update this list?
   - We would do this so the initial exchange of mempool transaction hashes can happen quickly, potentially without calling back to a trusted node.

## Current implementation
A `CapabilityServer` [is being implemented](./simple_capability.rs), along with a `clap` based tool to run the server [here](./sauron.rs).
Cool names for things are appreciated across the implementation, for example the client version and the binary name.

Low-level TODOs for the implementation:
 - [ ] Implement relaying
 - [ ] Implement basic alt chain logic and find chains (and status messages) which we would be able to reach with devp2p / eth.

## Design
There will be at least one type of privileged peer in this system - a node we will trust for answers to p2p queries each sniffer node will need to answer.

### Protocols for interacting with Ethereum
The Ethereum peer to peer network implements its own transport protocol called [RLPx](https://github.com/ethereum/devp2p/blob/master/rlpx.md), and nodes communicate using a protocol called [eth](https://github.com/ethereum/devp2p/blob/master/caps/eth.md).
Ethereum nodes also use [discv4](https://github.com/ethereum/devp2p/blob/master/discv4.md) and [discv5 (experimental afaik)](https://github.com/ethereum/devp2p/blob/master/discv5/discv5.md) for peer discovery.
This network of p2p listening nodes should be able to natively connect to a local node as a peer.
We should also provide a more specialized API for mempool related messages, for use in other applications.

#### Notes on rust devp2p implementations
The [ecies/rs](https://github.com/ecies/rs) implementation is not compatible, despite using secp256k1:
 - Uses HKDF-SHA256 instead of the [NIST SP 800-56 KDF](https://csrc.nist.gov/CSRC/media/Publications/sp/800-56a/archive/2006-05-03/documents/sp800-56-draft-jul2005.pdf).
 - Uses AES-GCM-256 instead of AES-CTR-128.
 - GCM vs CTR + HMAC

There are some existing libraries that let us interact with a peer on the network:
 - [The p2p library from OpenEthereum](https://github.com/openethereum/openethereum/tree/main/crates/net/network-devp2p/)
 - [Sentry](https://github.com/akula-bft/akula/tree/master/src/sentry)

OpenEthereum is likely unsuitable for this: It uses `parity-crypto`, which was [decommissioned here](https://github.com/paritytech/parity-common/commit/2d571df7fee92b85b47b49cf14aa3a7641f2f3b9).
If this library were replaced, it would need a new implementation of ECIES, and the only other compatible implementation is from [sentry](https://github.com/akula-bft/akula/tree/master/src/sentry/devp2p/src/ecies/algorithm.rs).

~~We can use [sentry's devp2p module](https://github.com/akula-bft/akula/tree/master/src/sentry/devp2p) for RLPx, and can handle eth messages separately.~~

Sentry was recently moved into the akula repo, and should be able to be used for interacting with the network.

### Gathering mempool transactions from multiple `p2p_txpool` instances
With multiple instances of `p2p_txpool`, it would be possible to create a stream of new transactions collected from each instance.
The goal of doing this would be to decrease the latency between the time the transaction is first broadcast, and when we finally receive the transaction.
This latency should be benchmarked.
These nodes should be located geographically close to their peers, to make sure that we are efficiently routing messages.

This general idea could also be used for listening to other types of eth p2p messages, such as `NewBlock`.
This would be useful for fast notification of new blocks, and could be combined with `p2p_txpool` to quickly remove transactions from a local node (or other application) mempool.
