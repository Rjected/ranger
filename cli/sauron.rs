use anvil::Hardfork;
use anyhow::Context;
use cidr::IpCidr;
use clap::Parser;
use devp2p_rs::{
    disc::dns::Resolver,
    v4::{Node, NodeRecord},
    CapabilityId, CapabilityName, CapabilityVersion, Discovery, Discv4, Discv4Builder,
    DnsDiscovery, ListenOptions, NodeRecord as RLPNodeRecord, StaticNodes, Swarm,
};
use ethp2p::{EthVersion, Status};
use foundry_config::Chain;
use hex_literal::hex;
use maplit::btreemap;
use ranger::relay::{MempoolListener, P2PRelay};
use ruint::uint;
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use std::{collections::HashMap, convert::TryInto};
use std::{num::NonZeroUsize, path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use task_group::TaskGroup;
use tokio::time::sleep;
use tokio_stream::{StreamExt, StreamMap};
use tracing::{info, trace, warn};
use tracing_subscriber::{
    prelude::__tracing_subscriber_SubscriberExt, util::SubscriberInitExt, EnvFilter,
};
use trust_dns_resolver::TokioAsyncResolver;

pub const BOOTNODES: &[&str] = &[
	"enode://d860a01f9722d78051619d1e2351aba3f43f943f6f00718d1b9baa4101932a1f5011f16bb2b1bb35db20d6fe28fa0bf09636d26a87d31de9ec6203eeedb1f666@18.138.108.67:30303",   // bootnode-aws-ap-southeast-1-001
	"enode://22a8232c3abc76a16ae9d6c3b164f98775fe226f0917b0ca871128a74a8e9630b458460865bab457221f1d448dd9791d24c4e5d88786180ac185df813a68d4de@3.209.45.79:30303",     // bootnode-aws-us-east-1-001
	"enode://ca6de62fce278f96aea6ec5a2daadb877e51651247cb96ee310a318def462913b653963c155a0ef6c7d50048bba6e6cea881130857413d9f50a621546b590758@34.255.23.113:30303",   // bootnode-aws-eu-west-1-001
	"enode://279944d8dcd428dffaa7436f25ca0ca43ae19e7bcf94a8fb7d1641651f92d121e972ac2e8f381414b80cc8e5555811c2ec6e1a99bb009b3f53c4c69923e11bd8@35.158.244.151:30303",  // bootnode-aws-eu-central-1-001
	"enode://8499da03c47d637b20eee24eec3c356c9a2e6148d6fe25ca195c7949ab8ec2c03e3556126b0d7ed644675e78c4318b08691b7b57de10e5f0d40d05b09238fa0a@52.187.207.27:30303",   // bootnode-azure-australiaeast-001
	"enode://103858bdb88756c71f15e9b5e09b56dc1be52f0a5021d46301dbbfb7e130029cc9d0d6f73f693bc29b665770fff7da4d34f3c6379fe12721b5d7a0bcb5ca1fc1@191.234.162.198:30303", // bootnode-azure-brazilsouth-001
	"enode://715171f50508aba88aecd1250af392a45a330af91d7b90701c436b618c86aaa1589c9184561907bebbb56439b8f8787bc01f49a7c77276c58c1b09822d75e8e8@52.231.165.108:30303",  // bootnode-azure-koreasouth-001
	"enode://5d6d7cd20d6da4bb83a1d28cadb5d409b64edf314c0335df658c1a54e32c7c4a7ab7823d57c39b6a757556e68ff1df17c748b698544a55cb488b52479a92b60f@104.42.217.25:30303",   // bootnode-azure-westus-001
];

#[derive(Parser)]
#[clap(
    name = "sauron",
    about = "Server that automatically connects to an eth p2p network, relaying messages to other nodes over RLPx."
)]
pub struct Opts {
    #[clap(long)]
    pub node_key: Option<String>,
    #[clap(long, default_value = "30303")]
    pub listen_port: u16,
    #[clap(long)]
    pub cidr: Option<IpCidr>,
    #[clap(long, default_value = "127.0.0.1:8000")]
    pub sentry_addr: String,
    #[clap(long, default_value = "all.mainnet.ethdisco.net")]
    pub dnsdisc_address: String,
    #[clap(long, default_value = "30303")]
    pub discv4_port: u16,
    #[clap(long)]
    pub discv4_bootnodes: Vec<NodeRecord>,
    #[clap(long, default_value = "1000")]
    pub discv4_cache: usize,
    #[clap(long, default_value = "1")]
    pub discv4_concurrent_lookups: usize,
    /// Peers that we will relay to
    #[clap(long)]
    pub relay_peers: Vec<RLPNodeRecord>,
    /// Peers whose responses will be trusted to relay to other peers, and influence certain
    /// relayer behavior
    #[clap(long)]
    pub trusted_peers: Vec<RLPNodeRecord>,
    #[clap(long)]
    pub static_peers: Vec<RLPNodeRecord>,
    #[clap(long, default_value = "5000")]
    pub static_peers_interval: u64,
    #[clap(long, default_value = "2500")]
    pub max_peers: NonZeroUsize,
    /// Disable DNS and UDP discovery, only use static peers.
    #[clap(long, takes_value = false)]
    pub no_discovery: bool,
    /// Disable DNS discovery
    #[clap(long, takes_value = false)]
    pub no_dns_discovery: bool,
    #[clap(long)]
    pub peers_file: Option<PathBuf>,
    #[clap(long, takes_value = false)]
    pub tokio_console: bool,
}

struct OptsDiscV4 {
    discv4_port: u16,
    discv4_bootnodes: Vec<NodeRecord>,
    discv4_cache: usize,
    discv4_concurrent_lookups: usize,
    listen_port: u16,
}

impl OptsDiscV4 {
    async fn make_task(self, secret_key: &SecretKey) -> anyhow::Result<Discv4> {
        info!("Starting discv4 at port {}", self.discv4_port);

        let mut bootstrap_nodes = self.discv4_bootnodes.into_iter().collect::<Vec<_>>();

        if bootstrap_nodes.is_empty() {
            bootstrap_nodes = BOOTNODES
                .iter()
                .map(|b| NodeRecord::from_str(b))
                .collect::<Result<Vec<_>, <NodeRecord as FromStr>::Err>>()?;
            info!("Using default discv4 bootstrap nodes");
        }

        let node = Node::new(
            format!("0.0.0.0:{}", self.discv4_port).parse().unwrap(),
            *secret_key,
            bootstrap_nodes,
            None,
            true,
            self.listen_port,
        )
        .await?;

        let task = Discv4Builder::default()
            .with_cache(self.discv4_cache)
            .with_concurrent_lookups(self.discv4_concurrent_lookups)
            .build(node);

        Ok(task)
    }
}

#[tokio::main]
/// TODO: a goal for this should be to simplify initialization s.t. it's something like this:
/// ```
/// // starts capability server, swarm, etc. responds to messages under the hood
/// let relay = Relay::new()
///               .status(status_message);
/// // or, with a version that peeks at status messages and will send the highest difficulty status
/// // we've seen so far. A peer could send us bogus status messages with high difficulty!
/// let relay = Relay::peeking_status();
/// // or, with a version that asks for headers, but doesn't verify blocks. just links together
/// // header hashes that a peer sends us so we can reconstruct a correct status message on our
/// // own, like SPV
/// let relay = Relay::spv_status();
/// // or with a trusted peer that we reach out to for things like the status and other p2p
/// // messages. would need to have the ip&port/enode/enr for that peer
/// let peer = get_trusted_peer();
/// let relay = Relay::with_trusted_peer(peer);
/// ```
async fn main() -> anyhow::Result<()> {
    let opts: Opts = Opts::parse();
    let filter = if std::env::var(EnvFilter::DEFAULT_ENV)
        .unwrap_or_default()
        .is_empty()
    {
        EnvFilter::new("sauron=trace,devp2p_rs=info,relay=debug")
    } else {
        EnvFilter::from_default_env()
    };
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    trace!("Relayer started with tracing enabled");

    let secret_key;
    if let Some(data) = opts.node_key {
        secret_key = SecretKey::from_slice(&hex::decode(data)?)?;
        info!("Loaded node key from config");
    } else {
        secret_key = SecretKey::new(&mut secp256k1::rand::thread_rng());
        info!("Generated new node key: {:?}", secret_key);
    };

    let listen_addr = format!("0.0.0.0:{}", opts.listen_port);

    info!("Starting Simple p2p relayer");

    info!(
        "Node ID: {}",
        hex::encode(
            devp2p_rs::util::pk2id(&PublicKey::from_secret_key(SECP256K1, &secret_key)).as_bytes()
        )
    );

    if let Some(cidr_filter) = &opts.cidr {
        info!("Peers restricted to range {}", cidr_filter);
    }

    let mut discovery_tasks: StreamMap<String, Discovery> = StreamMap::new();

    if !opts.no_discovery {
        if !opts.no_dns_discovery {
            info!("Starting DNS discovery fetch from {}", opts.dnsdisc_address);

            let dns_resolver = Resolver::new(Arc::new(
                TokioAsyncResolver::tokio_from_system_conf()
                    .context("Failed to start DNS resolver")?,
            ));
            let task = DnsDiscovery::new(Arc::new(dns_resolver), opts.dnsdisc_address, None);
            discovery_tasks.insert("dnsdisc".to_string(), Box::pin(task));
        }

        let task_opts = OptsDiscV4 {
            discv4_port: opts.discv4_port,
            discv4_bootnodes: opts.discv4_bootnodes,
            discv4_cache: opts.discv4_cache,
            discv4_concurrent_lookups: opts.discv4_concurrent_lookups,
            listen_port: opts.listen_port,
        };
        let task = task_opts.make_task(&secret_key).await?;

        discovery_tasks.insert("discv4".to_string(), Box::pin(task));
    }

    if !opts.static_peers.is_empty() {
        info!("Enabling static peers: {:?}", opts.static_peers);

        let task = StaticNodes::new(
            opts.static_peers
                .iter()
                .map(|&RLPNodeRecord { addr, id, .. }| (addr, id))
                .collect::<HashMap<_, _>>(),
            Duration::from_millis(opts.static_peers_interval),
        );
        discovery_tasks.insert("static peers".to_string(), Box::pin(task));
    }

    if discovery_tasks.is_empty() {
        warn!("All discovery methods are disabled, we will not search for peers.");
    }

    let tasks = Arc::new(TaskGroup::new());

    let status = Status {
        version: EthVersion::Eth67 as u8,
        chain: Chain::Id(1),
        total_difficulty: uint!(36206751599115524359527_U256),
        blockhash: hex!("feb27336ca7923f8fab3bd617fcb6e75841538f71c1bcfc267d7838489d9e13d"),
        genesis: hex!("d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3"),
        forkid: Hardfork::Latest.fork_id(),
    };

    // tell the relay to use this status message
    let relay = P2PRelay::new().with_status(status);
    let relay = Arc::new(relay);
    let no_new_peers = relay.no_new_peers_handle();

    let swarm = Swarm::builder()
        .with_task_group(tasks.clone())
        .with_listen_options(ListenOptions::new(
            discovery_tasks,
            opts.max_peers.into(), // if num_peers > min_peers, then there is only one dial worker (this
                            // is an implementation detail of the swarm). so let's set min_peers
                            // very high, so we actually USE dialer tasks rather than have only one
                            // worker.
                            // this is one issue i have with designing the devp2p api around task
                            // groups - it exposes too many implementation details about
                            // the concurrency paradigm to the user.
                            // min_peers also seems useless, because that's all it's used for.
            opts.max_peers,
            listen_addr.parse().unwrap(),
            opts.cidr,
            no_new_peers, // this should be changed to be static - are there good reasons for it not being static?
        ))
        .with_client_version(format!("sneakyboi/v{}", env!("CARGO_PKG_VERSION")))
        .build(
            btreemap! {
                CapabilityId { name: CapabilityName("eth".try_into().unwrap()), version: EthVersion::Eth66 as CapabilityVersion } => 17,
                CapabilityId { name: CapabilityName("eth".try_into().unwrap()), version: EthVersion::Eth67 as CapabilityVersion } => 15, // on bsc this is 18 lol
            },
            relay.clone(),
            secret_key,
        )
        .await
        .context("Failed to start RLPx node")?;

    info!("RLPx node listening at {}", listen_addr);

    // let's just keep waiting for transactions
    let mut tx_stream = swarm.subscribe_pending_txs().unwrap();
    let mut hashes_stream = swarm.subscribe_pending_hashes().unwrap();

    let mut counter: u32 = 0;
    loop {
        counter += 1;
        if counter == 3000 {
            info!(
                "Peer info: {} active (+{} dialing) / {} max.",
                swarm.connected_peers(),
                swarm.dialing(),
                opts.max_peers
            );
            counter = 0;
        }

        while let Some(hash) = hashes_stream.next().await {
            info!("New tx hash! {:?}", hex::encode(hash.unwrap()))
        }

        while let Some(new_tx) = tx_stream.next().await {
            info!("New tx! {:?}", new_tx.unwrap())
        }

        sleep(Duration::from_millis(20)).await;
    }
}
