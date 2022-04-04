# Sauron

Sauron is a program that connects to the ethereum network and relays p2p messages.

Currently the [`Status`](https://github.com/ethereum/devp2p/blob/master/caps/eth.md#status-0x00) message is hardcoded, and we are disconnected from other nodes quickly after we connect.

## How to run
Once in the `cli/` directory:
```
cargo run --
```
