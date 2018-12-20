extern crate chrono;
#[macro_use]
extern crate clap;
extern crate env_logger;
extern crate failure;
extern crate futures;
extern crate hex;
extern crate itertools;
#[macro_use]
extern crate log;
extern crate regex;
#[cfg(unix)]
extern crate syslog;
extern crate tokio;
extern crate tokio_codec;
extern crate config;
#[macro_use]
extern crate serde_derive;
extern crate serde;
extern crate serde_ignored;
extern crate serde_yaml;
extern crate tox;

mod node_config;
mod motd;

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

use failure::Error;
use futures::sync::mpsc;
use futures::{future, Future, Stream};
use futures::future::Either;
use itertools::Itertools;
use log::LevelFilter;
use tokio::net::{TcpListener, UdpSocket};
use tokio::runtime;
use tox::toxcore::crypto_core::*;
use tox::toxcore::dht::server::{Server as UdpServer};
use tox::toxcore::dht::server_ext::{ServerExt as UdpServerExt};
use tox::toxcore::dht::lan_discovery::LanDiscoverySender;
use tox::toxcore::onion::packet::InnerOnionResponse;
use tox::toxcore::tcp::packet::OnionRequest;
use tox::toxcore::tcp::server::{Server as TcpServer, ServerExt as TcpServerExt};
use tox::toxcore::stats::Stats;
#[cfg(unix)]
use syslog::Facility;

use node_config::*;
use motd::{Motd, Counters};

/// Channel size for onion messages between UDP and TCP relay.
const ONION_CHANNEL_SIZE: usize = 32;
/// Channel size for DHT packets.
const DHT_CHANNEL_SIZE: usize = 32;

/// Get version in format 3AAABBBCCC, where A B and C are major, minor and patch
/// versions of node. `tox-bootstrapd` uses similar scheme but with leading 1.
/// Before it used format YYYYMMDDVV so the leading numeral was 2. To make a
/// difference with these schemes we use 3.
fn version() -> u32 {
    let major: u32 = env!("CARGO_PKG_VERSION_MAJOR").parse().expect("Invalid major version");
    let minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().expect("Invalid minor version");
    let patch: u32 = env!("CARGO_PKG_VERSION_PATCH").parse().expect("Invalid patch version");
    assert!(major < 1000, "Invalid major version");
    assert!(minor < 1000, "Invalid minor version");
    assert!(patch < 1000, "Invalid patch version");
    3000000000 + major * 1000000 + minor * 1000 + patch
}

/// Bind a UDP listener to the socket address.
fn bind_socket(addr: SocketAddr) -> UdpSocket {
    let socket = UdpSocket::bind(&addr).expect("Failed to bind UDP socket");
    socket.set_broadcast(true).expect("set_broadcast call failed");
    if addr.is_ipv6() {
        socket.set_multicast_loop_v6(true).expect("set_multicast_loop_v6 call failed");
    }
    socket
}

/// Save DHT keys to a binary file.
fn save_keys(keys_file: &str, pk: PublicKey, sk: &SecretKey) {
    #[cfg(not(unix))]
    let mut file = File::create(keys_file).expect("Failed to create the keys file");

    #[cfg(unix)]
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .mode(0o600)
        .open(keys_file)
        .expect("Failed to create the keys file");

    file.write_all(pk.as_ref()).expect("Failed to save public key to the keys file");
    file.write_all(&sk[0..SECRETKEYBYTES]).expect("Failed to save secret key to the keys file");
}

/// Load DHT keys from a binary file.
fn load_keys(mut file: File) -> (PublicKey, SecretKey) {
    let mut buf = [0; PUBLICKEYBYTES + SECRETKEYBYTES];
    file.read_exact(&mut buf).expect("Failed to read keys from the keys file");
    let pk = PublicKey::from_slice(&buf[..PUBLICKEYBYTES]).expect("Failed to read public key from the keys file");
    let sk = SecretKey::from_slice(&buf[PUBLICKEYBYTES..]).expect("Failed to read secret key from the keys file");
    assert!(pk == sk.public_key(), "The loaded public key does not correspond to the loaded secret key");
    (pk, sk)
}

/// Load DHT keys from a binary file or generate and save them if file does not
/// exist.
fn load_or_gen_keys(keys_file: &str) -> (PublicKey, SecretKey) {
    match File::open(keys_file) {
        Ok(file) => load_keys(file),
        Err(ref e) if e.kind() == ErrorKind::NotFound => {
            info!("Generating new DHT keys and storing them to '{}'", keys_file);
            let (pk, sk) = gen_keypair();
            save_keys(keys_file, pk, &sk);
            (pk, sk)
        },
        Err(e) => panic!("Failed to read the keys file: {}", e)
    }
}

/// Run a future with the runtime specified by config.
fn run<F>(future: F, threads: Threads)
    where F: Future<Item = (), Error = Error> + Send + 'static
{
    if threads == Threads::N(1) {
        let mut runtime = runtime::current_thread::Runtime::new().expect("Failed to create runtime");
        runtime.block_on(future).expect("Execution was terminated with error");
    } else {
        let mut builder = runtime::Builder::new();
        builder.name_prefix("tox-node-");
        match threads {
            Threads::N(n) => { builder.core_threads(n as usize); },
            Threads::Auto => { }, // builder will detect number of cores automatically
        }
        let mut runtime = builder
            .build()
            .expect("Failed to create runtime");
        runtime.block_on(future).expect("Execution was terminated with error");
    };
}

/// Onion sink and stream for TCP.
struct TcpOnion {
    /// Sink for onion packets from TCP to UDP.
    tx: mpsc::Sender<(OnionRequest, SocketAddr)>,
    /// Stream of onion packets from TCP to UDP.
    rx: mpsc::Receiver<(InnerOnionResponse, SocketAddr)>,
}

/// Onion sink and stream for UDP.
struct UdpOnion {
    /// Sink for onion packets from UDP to TCP.
    tx: mpsc::Sender<(InnerOnionResponse, SocketAddr)>,
    /// Stream of onion packets from TCP to UDP.
    rx: mpsc::Receiver<(OnionRequest, SocketAddr)>,
}

/// Create onion streams for TCP and UDP servers communication.
fn create_onion_streams() -> (TcpOnion, UdpOnion) {
    let (udp_onion_tx, udp_onion_rx) = mpsc::channel(ONION_CHANNEL_SIZE);
    let (tcp_onion_tx, tcp_onion_rx) = mpsc::channel(ONION_CHANNEL_SIZE);
    let tcp_onion = TcpOnion {
        tx: tcp_onion_tx,
        rx: udp_onion_rx,
    };
    let udp_onion = UdpOnion {
        tx: udp_onion_tx,
        rx: tcp_onion_rx,
    };
    (tcp_onion, udp_onion)
}

fn run_tcp(config: &NodeConfig, dht_sk: SecretKey, tcp_onion: TcpOnion, stats: Stats) -> impl Future<Item = (), Error = Error> {
    if config.tcp_addrs.is_empty() {
        // If TCP address is not specified don't start TCP server and only drop
        // all onion packets from DHT server
        let tcp_onion_future = tcp_onion.rx
            .map_err(|()| unreachable!("rx can't fail"))
            .for_each(|_| future::ok(()));
        return Either::A(tcp_onion_future)
    }

    let mut tcp_server = TcpServer::new();
    tcp_server.set_udp_onion_sink(tcp_onion.tx);

    let tcp_server_c = tcp_server.clone();
    let tcp_server_futures = config.tcp_addrs.iter().map(move |&addr| {
        let tcp_server_c = tcp_server_c.clone();
        let dht_sk = dht_sk.clone();
        let listener = TcpListener::bind(&addr).expect("Failed to bind TCP listener");
        tcp_server_c.run(listener, dht_sk, stats.clone())
            .map_err(Error::from)
    });

    let tcp_server_future = future::select_all(tcp_server_futures)
        .map(|_| ())
        .map_err(|(e, _, _)| e);

    let tcp_onion_future = tcp_onion.rx
        .map_err(|()| unreachable!("rx can't fail"))
        .for_each(move |(onion_response, addr)|
            tcp_server.handle_udp_onion_response(addr.ip(), addr.port(), onion_response).or_else(|err| {
                warn!("Failed to handle UDP onion response: {:?}", err);
                future::ok(())
            })
        );

    info!("Running TCP relay on {}", config.tcp_addrs.iter().format(","));

    Either::B(tcp_server_future
        .join(tcp_onion_future)
        .map(|_| ()))
}

fn run_udp(config: &NodeConfig, dht_pk: PublicKey, dht_sk: &SecretKey, udp_onion: UdpOnion, tcp_stats: Stats) -> impl Future<Item = (), Error = Error> {
    let udp_addr = if let Some(udp_addr) = config.udp_addr {
        udp_addr
    } else {
        // If UDP address is not specified don't start DHT server and only drop
        // all onion packets from TCP server
        let udp_onion_future = udp_onion.rx
            .map_err(|()| unreachable!("rx can't fail"))
            .for_each(|_| future::ok(()));
        return Either::A(udp_onion_future)
    };

    let socket = bind_socket(udp_addr);
    let udp_stats = Stats::new();

    // Create a channel for server to communicate with network
    let (tx, rx) = mpsc::channel(DHT_CHANNEL_SIZE);

    let lan_discovery_future = if config.lan_discovery_enabled {
        Either::A(LanDiscoverySender::new(tx.clone(), dht_pk, udp_addr.is_ipv6())
            .run()
            .map_err(Error::from))
    } else {
        Either::B(future::empty())
    };

    let mut udp_server = UdpServer::new(tx, dht_pk, dht_sk.clone());
    let counters = Counters::new(tcp_stats, udp_stats.clone());
    let motd = Motd::new(config.motd.clone(), counters);
    udp_server.set_bootstrap_info(version(), Box::new(move |_| motd.format().as_bytes().to_owned()));
    udp_server.enable_lan_discovery(config.lan_discovery_enabled);
    udp_server.set_tcp_onion_sink(udp_onion.tx);
    udp_server.enable_ipv6_mode(udp_addr.is_ipv6());

    let udp_server_c = udp_server.clone();
    let udp_onion_future = udp_onion.rx
        .map_err(|()| unreachable!("rx can't fail"))
        .for_each(move |(onion_request, addr)|
            udp_server_c.handle_tcp_onion_request(onion_request, addr).or_else(|err| {
                warn!("Failed to handle TCP onion request: {:?}", err);
                future::ok(())
            })
        );

    if config.bootstrap_nodes.is_empty() {
        warn!("No bootstrap nodes!");
    }

    for node in config.bootstrap_nodes.iter().flat_map(|node| node.resolve()) {
        udp_server.add_initial_bootstrap(node);
    }

    info!("Running DHT server on {}", udp_addr);

    Either::B(udp_server.run_socket(socket, rx, udp_stats).map_err(Error::from)
        .select(lan_discovery_future).map(|_| ()).map_err(|(e, _)| e)
        .join(udp_onion_future).map(|_| ()))
}

fn main() {
    if !crypto_init() {
        panic!("Crypto initialization failed.");
    }

    let config = cli_parse();

    match config.log_type {
        LogType::Stderr => {
            let env = env_logger::Env::default()
                .filter_or("RUST_LOG", "info");
            env_logger::Builder::from_env(env)
                .init();
        },
        LogType::Stdout => {
            let env = env_logger::Env::default()
                .filter_or("RUST_LOG", "info");
            env_logger::Builder::from_env(env)
                .target(env_logger::fmt::Target::Stdout)
                .init();
        },
        #[cfg(unix)]
        LogType::Syslog => {
            syslog::init(Facility::LOG_USER, LevelFilter::Info, None)
                .expect("Failed to initialize syslog backend.");
        },
        LogType::None => { },
    }

    for arg_unused in config.unused.clone() {
        warn!("Unused configuration key: {:?}", arg_unused);
    }

    let (dht_pk, dht_sk) = if let Some(ref sk) = config.sk {
        (sk.public_key(), sk.clone())
    } else if let Some(ref keys_file) = config.keys_file {
        load_or_gen_keys(keys_file)
    } else {
        panic!("Neither secret key nor keys file is specified")
    };

    if config.tcp_addrs.is_empty() && config.udp_addr.is_none() {
        panic!("Both TCP addresses and UDP address are not defined.")
    }

    if config.sk_passed_as_arg {
        warn!("You should not pass the secret key via arguments due to \
               security reasons. Use the environment variable instead");
    }

    info!("DHT public key: {}", hex::encode(dht_pk.as_ref()).to_uppercase());

    let (tcp_onion, udp_onion) = create_onion_streams();

    let tcp_stats = Stats::new();
    let udp_server_future = run_udp(&config, dht_pk, &dht_sk, udp_onion, tcp_stats.clone());
    let tcp_server_future = run_tcp(&config, dht_sk, tcp_onion, tcp_stats);

    let future = udp_server_future.select(tcp_server_future).map(|_| ()).map_err(|(e, _)| e);

    run(future, config.threads);
}
