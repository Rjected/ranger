# <h1 align="center"> ranger </h1>

![Github Actions](https://github.com/Rjected/ranger/workflows/Tests/badge.svg)

Ranger is a set of utilities for listening to the [peer to peer network](https://github.com/ethereum/devp2p/blob/master/caps/eth.md) without a node, serving mempool transactions and other data from the eth p2p protocol.

### <p align="center"> ⚠️🚧 WIP 🚧⚠️ </p>

Ranger is separated into two parts currently:
 * [**sauron**](./cli/) provides a command line program which will print out
     incoming transactions and transaction hashes as it receives them from the
     network. This needs a lot of work before it can remain connected to other
     nodes! This is a work in progress!
 * [**relay**](./relay/) includes utility code for interacting with the p2p
     network, and traits for exposing mempool transactions as a `Stream`.

## Related projects
 * [ethp2p](https://github.com/rjected/ethp2p), a library built for ranger, which is used for encoding and decoding [`eth`](https://github.com/ethereum/devp2p) messages.
 * [Sentry](https://github.com/akula-bft/akula/tree/master/src/sentry), which provides much of the code that we use to interact with eth p2p networking protocols.
