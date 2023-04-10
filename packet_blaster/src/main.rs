use std::{
    fs, io,
    io::ErrorKind,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::Sub,
    path::PathBuf,
    sync::Arc,
    thread,
    thread::Builder,
    time::{Duration, Instant},
};

use bincode::serialize;
use clap::Parser;
use itertools::Itertools;
use log::*;
use once_cell::sync::Lazy;
use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    connection_cache::ConnectionCacheStats,
    nonblocking::quic_client::QuicLazyInitializedEndpoint,
    quic_client::QuicTpuConnection,
    rpc_client::RpcClient,
    tpu_connection::TpuConnection,
};
use solana_sdk::{
    native_token::LAMPORTS_PER_SOL,
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    system_transaction::transfer,
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// RPC address
    #[arg(long, env, default_value = "http://127.0.0.1:8899")]
    rpc_addr: String,

    /// Path to signer+payer keypairs
    #[arg(long, env)]
    keypair_path: PathBuf,

    /// Socket address for relayer TPU
    #[arg(long, env, default_value = "127.0.0.1:8009")]
    tpu_addr: SocketAddr,

    /// Offset starting ip and port to allow more instances of packet blaster to run on the same machine
    #[arg(long, env, default_value_t = 0)]
    ip_port_offset: u16,

    /// Interval between sending packets on a given thread
    #[arg(long, env)]
    loop_sleep_micros: Option<u64>,

    /// Method of connecting to Solana TPU
    #[command(subcommand)]
    connection_mode: Mode,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum Mode {
    /// Solana Quic
    Quic,

    /// Quinn client
    Quinn {
        /// Only works from localhost relative to relayer.
        /// Creates many 127.x.x.x addresses to overwhelm relayer.
        #[arg(long, env)]
        spam_from_localhost: bool,
    },
    /// Slow loris
    SlowLoris {
        /// Only works from localhost relative to relayer.
        /// Creates many 127.x.x.x addresses to overwhelm relayer.
        #[arg(long, env)]
        spam_from_localhost: bool,

        /// Slow loris, sleep delay between bytes.
        /// taken from https://github.com/solana-labs/solana/blob/cd6ba30cb0f990079a3d22e62d4f7f315ede4ce4/streamer/src/nonblocking/quic.rs#L42
        #[arg(long, env, default_value_t = 50)]
        sleep_interval_ms: u64,

        /// Number of connections per IP. //FIXME not hooked up
        #[arg(long, env, default_value_t = 8)]
        num_connections: u64,

        /// Number of streams per connection
        #[arg(long, env, default_value_t = solana_sdk::quic::QUIC_MAX_UNSTAKED_CONCURRENT_STREAMS as u64)]
        num_streams_per_conn: u64,
    },
}
/// Doc comment
// #[derive(clap::Args, Debug, Clone)]
// #[group(requires_all = true)]
// struct SlowLoris {}

fn read_keypairs(path: PathBuf) -> io::Result<Vec<Keypair>> {
    if path.is_dir() {
        let result = fs::read_dir(path)?
            .filter_map(|entry| solana_sdk::signature::read_keypair_file(entry.ok()?.path()).ok())
            .collect::<Vec<_>>();
        Ok(result)
    } else {
        Ok(vec![Keypair::from_bytes(&fs::read(path)?).map_err(
            |e| io::Error::new(ErrorKind::NotFound, e.to_string()),
        )?])
    }
}

const TXN_BATCH_SIZE: u64 = 10;

/// Generates sequential localhost sockets on different IPs
pub fn local_socket_addr(
    ip_port_offset: u16,
    thread_id: usize,
    spam_from_localhost: bool,
) -> SocketAddr {
    let offset = ip_port_offset as u32 + thread_id as u32;
    let ip: [u8; 4] = offset.to_be_bytes();
    let port = 1024 + offset as u16;
    match spam_from_localhost {
        true => SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, ip[1], ip[2], ip[3])), port),
        false => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port), /* for sending from remote machine */
    }
}

static RUNTIME: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
});

fn main() {
    env_logger::init();

    let args: Args = Args::parse();
    dbg!(&args);

    let keypairs = read_keypairs(args.keypair_path).expect("Failed to read keypairs");
    let pubkeys = keypairs
        .iter()
        .map(|kp| kp.pubkey())
        .collect::<Vec<Pubkey>>();

    let starting_port = 1024 + args.ip_port_offset;
    info!(
        "Packet blaster will send on ports {}..={} with {} pubkeys: {pubkeys:?}",
        starting_port,
        starting_port + pubkeys.len() as u16,
        pubkeys.len()
    );

    let threads: Vec<_> = keypairs
        .into_iter()
        .enumerate()
        .map(|(thread_id, keypair)| {
            let client = Arc::new(RpcClient::new(&args.rpc_addr));
            let connection_mode = args.connection_mode.clone();
            Builder::new()
                .name(format!("packet_blaster-thread_{thread_id}"))
                .spawn(move || {
                    let tpu_sender = match RUNTIME.block_on(TpuSender::new(
                        &connection_mode,
                        args.tpu_addr,
                        args.ip_port_offset,
                        thread_id,
                    )) {
                        Ok(x) => x,
                        Err(e) => panic!("Failed to connect, err: {e}"),
                    };
                    let metrics_interval = Duration::from_secs(5);
                    let mut last_blockhash_refresh = Instant::now();
                    let mut latest_blockhash = client.get_latest_blockhash().unwrap();
                    let mut curr_txn_count = 0u64;
                    let mut curr_fail_count = 0u64;
                    let mut cumm_txn_count = 0u64;
                    let mut cumm_fail_count = 0u64;
                    loop {
                        let now = Instant::now();
                        let elapsed = now.sub(last_blockhash_refresh);
                        if elapsed > metrics_interval {
                            cumm_txn_count += curr_txn_count;
                            cumm_fail_count += curr_fail_count;
                            info!(
                                "thread_{thread_id} txn/sec: {:.0}, \
                                success: {}, \
                                fail: {curr_fail_count}, \
                                total txn: {cumm_txn_count}, \
                                total success %: {:.1}",
                                (curr_txn_count) as f64 / elapsed.as_secs_f64(),
                                curr_txn_count - curr_fail_count,
                                (1.0 - (cumm_fail_count as f64 / cumm_txn_count as f64)) * 100.0
                            );
                            last_blockhash_refresh = now;

                            curr_txn_count = 0;
                            curr_fail_count = 0;
                            latest_blockhash = client.get_latest_blockhash().unwrap();
                        }

                        let serialized_txns: Vec<Vec<u8>> = (0..TXN_BATCH_SIZE)
                            .filter_map(|i| {
                                let lamports = cumm_txn_count + curr_txn_count + i;
                                let txn = transfer(
                                    &keypair,
                                    &keypair.pubkey(),
                                    lamports,
                                    latest_blockhash,
                                );
                                // debug!(
                                //     "pubkey: {}, lamports: {}, signature: {:?}",
                                //     &keypair.pubkey(),
                                //     lamports,
                                //     &txn.signatures
                                // );
                                serialize(&txn).ok()
                            })
                            .collect();
                        let txn_count = serialized_txns.len() as u64;
                        curr_txn_count += txn_count;
                        let (_successes, fails): (Vec<()>, Vec<PacketBlasterError>) = RUNTIME
                            .block_on(tpu_sender.send(serialized_txns))
                            .into_iter()
                            .partition_result();

                        curr_fail_count += fails.len() as u64;

                        if let Some(dur) = args.loop_sleep_micros {
                            thread::sleep(Duration::from_micros(dur))
                        }
                    }
                })
                .unwrap()
        })
        .collect();

    for t in threads {
        t.join().unwrap();
    }
}

enum TpuSender {
    Quinn {
        connection: quinn::Connection,
    },
    SlowLoris {
        connection: Arc<quinn::Connection>,
        num_streams_per_conn: u64,
        sleep_interval: Duration,
    },
    Quic {
        client: QuicTpuConnection,
    },
}

// taken from https://github.com/solana-labs/solana/blob/527e2d4f59c6429a4a959d279738c872b97e56b5/client/src/nonblocking/quic_client.rs#L42
struct SkipServerVerification;

impl SkipServerVerification {
    pub fn new() -> Arc<Self> {
        Arc::new(Self)
    }
}

impl rustls::client::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::Certificate,
        _intermediates: &[rustls::Certificate],
        _server_name: &rustls::ServerName,
        _scts: &mut dyn Iterator<Item = &[u8]>,
        _ocsp_response: &[u8],
        _now: std::time::SystemTime,
    ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::ServerCertVerified::assertion())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PacketBlasterError {
    #[error("connect error: {0}")]
    ConnectError(#[from] quinn::ConnectError),
    #[error("connection error: {0}")]
    ConnectionError(#[from] quinn::ConnectionError),
    #[error("write error: {0}")]
    WriteError(#[from] quinn::WriteError),
    #[error("transport error: {0}")]
    TransportError(#[from] solana_sdk::transport::TransportError),
}

impl TpuSender {
    // source taken from https://github.com/solana-labs/solana/blob/527e2d4f59c6429a4a959d279738c872b97e56b5/client/src/nonblocking/quic_client.rs#L93
    // original code doesn't allow specifying source socket
    fn create_endpoint(send_addr: SocketAddr) -> quinn::Endpoint {
        let (certs, priv_key) =
            solana_streamer::tls_certificates::new_self_signed_tls_certificate_chain(
                &Keypair::new(),
                send_addr.ip(),
            )
            .expect("Failed to create QUIC client certificate");
        let mut crypto = rustls::ClientConfig::builder()
            .with_safe_defaults()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_single_cert(certs, priv_key)
            .expect("Failed to set QUIC client certificates");
        crypto.enable_early_data = true;
        crypto.alpn_protocols =
            vec![solana_streamer::nonblocking::quic::ALPN_TPU_PROTOCOL_ID.to_vec()];

        let mut endpoint = quinn::Endpoint::client(send_addr).unwrap();
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));

        let mut transport_config = quinn::TransportConfig::default();
        let timeout = quinn::IdleTimeout::from(quinn::VarInt::from_u32(
            solana_sdk::quic::QUIC_MAX_TIMEOUT_MS * 100, /* Hack for when relayer is backed up and not accepting connections */
        ));
        transport_config.max_idle_timeout(Some(timeout));
        transport_config.keep_alive_interval(Some(Duration::from_millis(
            solana_sdk::quic::QUIC_KEEP_ALIVE_MS,
        )));
        config.transport_config(Arc::new(transport_config));

        endpoint.set_default_client_config(config);
        endpoint
    }

    async fn new(
        connection_mode: &Mode,
        dest_addr: SocketAddr,
        ip_port_offset: u16,
        thread_id: usize,
    ) -> Result<TpuSender, PacketBlasterError> {
        match connection_mode {
            Mode::Quinn {
                spam_from_localhost,
            } => {
                let connection = Self::setup_quinn_sender(
                    dest_addr,
                    ip_port_offset,
                    thread_id,
                    spam_from_localhost,
                )
                .await;
                Ok(TpuSender::Quinn { connection })
            }
            Mode::SlowLoris {
                spam_from_localhost,
                sleep_interval_ms,
                num_streams_per_conn,
                num_connections: _num_connections,
            } => {
                let connection = Self::setup_quinn_sender(
                    dest_addr,
                    ip_port_offset,
                    thread_id,
                    spam_from_localhost,
                )
                .await;
                Ok(TpuSender::SlowLoris {
                    connection: Arc::new(connection),
                    num_streams_per_conn: *num_streams_per_conn,
                    sleep_interval: Duration::from_millis(*sleep_interval_ms),
                })
            }
            Mode::Quic => Ok(TpuSender::Quic {
                client: QuicTpuConnection::new(
                    Arc::new(QuicLazyInitializedEndpoint::default()),
                    dest_addr,
                    Arc::new(ConnectionCacheStats::default()),
                ),
            }),
        }
    }

    async fn setup_quinn_sender(
        dest_addr: SocketAddr,
        ip_port_offset: u16,
        thread_id: usize,
        spam_from_localhost: &bool,
    ) -> quinn::Connection {
        let send_socket_addr = local_socket_addr(ip_port_offset, thread_id, *spam_from_localhost);
        let endpoint = Self::create_endpoint(send_socket_addr);
        // Connect to the server passing in the server name which is supposed to be in the server certificate.
        let connection = endpoint
            .connect(dest_addr, "connect")
            .unwrap()
            .await
            .unwrap_or_else(|_| {
                panic!("Failed to bind thread_{thread_id} to {send_socket_addr:?}")
            });
        info!("Sending thread_{thread_id} packets on {send_socket_addr:?}");

        connection
    }

    async fn send(&self, serialized_txns: Vec<Vec<u8>>) -> Vec<Result<(), PacketBlasterError>> {
        match self {
            TpuSender::Quinn { connection } => {
                let futures = serialized_txns.into_iter().map(|buf| async move {
                    let mut send_stream = connection.open_uni().await?;
                    send_stream.write_all(&buf).await?;
                    send_stream.finish().await?;
                    Ok::<(), PacketBlasterError>(())
                });

                let results: Vec<Result<(), PacketBlasterError>> =
                    futures_util::future::join_all(futures).await;

                results
            }
            TpuSender::SlowLoris {
                connection,
                sleep_interval,
                num_streams_per_conn: _num_streams_per_conn,
            } => {
                let futures = serialized_txns.into_iter().map(|txn| async move {
                    //TODO: use all args.num_stream_count
                    let mut send_stream = connection
                        .open_uni()
                        .await
                        .map_err(PacketBlasterError::ConnectionError)?;
                    let stream_id = 0;
                    // Send a full size packet with single byte writes sequentially
                    for chunk in txn.chunks(2) {
                        // info!("sending byte[{i}] for stream_{stream_id}");
                        send_stream.write_all(chunk).await?;
                        tokio::time::sleep(*sleep_interval).await;
                    }
                    send_stream.finish().await?;

                    Ok::<(), PacketBlasterError>(())
                });

                let results: Vec<Result<(), PacketBlasterError>> =
                    futures_util::future::join_all(futures).await;
                results
            }
            TpuSender::Quic { client } => {
                vec![client
                    .send_wire_transaction_batch_async(serialized_txns)
                    .map_err(PacketBlasterError::TransportError)]
            }
        }
    }

    /// Breaks up a txn into 1 byte sized writes over a quic stream
    async fn stream_txn_slowly(
        connection: Arc<quinn::Connection>,
        txn: Vec<u8>,
        sleep_interval: Duration,
        stream_id: u64,
    ) -> Result<(), PacketBlasterError> {
        let mut send_stream = connection
            .open_uni()
            .await
            .map_err(PacketBlasterError::ConnectionError)?;

        // Send a full size packet with single byte writes sequentially
        // chunk size 1 gets rejected
        for chunk in txn.chunks(2) {
            send_stream.write_all(chunk).await?;
            tokio::time::sleep(sleep_interval).await;
        }
        send_stream.finish().await?;
        Ok(())
    }
}

#[allow(unused)]
fn request_and_confirm_airdrop(
    client: &RpcClient,
    pubkeys: &[Pubkey],
) -> solana_client::client_error::Result<()> {
    let sigs = pubkeys
        .iter()
        .map(|pubkey| client.request_airdrop(pubkey, 100 * LAMPORTS_PER_SOL))
        .collect::<solana_client::client_error::Result<Vec<Signature>>>()?;

    let now = Instant::now();
    while now.elapsed() < Duration::from_secs(20) {
        let r = client.get_signature_statuses(&sigs)?;
        if r.value.iter().all(|s| s.is_some()) {
            return Err(ClientError::from(ClientErrorKind::Custom(
                "signature error".to_string(),
            )));
        }
    }
    Ok(())
}
