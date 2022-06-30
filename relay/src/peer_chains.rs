use std::collections::{hash_map::Entry, HashMap, HashSet};

use akula::p2p::node::PeerId;
use foundry_config::Chain;

/// Data structure which maps a peer to its chain, and the chain to the set of peers on that chain.
#[derive(Debug, Clone, Default)]
pub struct PeerChains {
    /// Map of a peer to a chain.
    pub peer_chains: HashMap<PeerId, Chain>,
    /// Map of a chain to a set of peers on that chain.
    pub chain_peers: HashMap<Chain, HashSet<PeerId>>,
}

impl PeerChains {
    /// Add a peer to a chain.
    pub fn add_peer(&mut self, peer: PeerId, chain: Chain) {
        self.peer_chains.insert(peer, chain);
        self.chain_peers
            .entry(chain)
            .or_insert_with(HashSet::new)
            .insert(peer);
    }

    /// Remove a peer from a chain.
    pub fn remove_peer(&mut self, peer: PeerId) {
        if let Some(chain) = self.peer_chains.get(&peer) {
            if let Entry::Occupied(mut entry) = self.chain_peers.entry(*chain) {
                entry.get_mut().remove(&peer);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
        }
        self.peer_chains.remove(&peer);
    }

    /// Get the chain of a peer.
    pub fn get_chain(&self, peer: &PeerId) -> Option<&Chain> {
        self.peer_chains.get(peer)
    }

    /// Get the list of peers on a chain.
    pub fn get_peers(&self, chain: &Chain) -> Option<&HashSet<PeerId>> {
        self.chain_peers.get(chain)
    }

    /// Determine if a peer is on a chain.
    pub fn is_on_chain(&self, peer: &PeerId, chain: &Chain) -> bool {
        self.peer_chains.get(peer) == Some(chain)
    }
}
