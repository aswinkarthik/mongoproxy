use std::convert::Infallible;
use std::net::{SocketAddr,ToSocketAddrs};
use std::io;
use std::str;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener,TcpStream};
use tokio::net::tcp::{OwnedReadHalf,OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::StreamReader;

use prometheus::{Counter,CounterVec,HistogramVec,Encoder,TextEncoder};
use clap::{Arg, App, crate_version};
use tracing::{info_span, Instrument, Level};
use tracing_subscriber::{FmtSubscriber, EnvFilter};
use lazy_static::lazy_static;
use tokio_native_tls::{TlsConnector, TlsStream};
use native_tls::{TlsConnector as NativeTlsConnector, Protocol};

#[macro_use] extern crate prometheus;

use hyper::{
    service::{make_service_fn, service_fn},
    Body, Method, Request, Response, Server, StatusCode,
};

use mongoproxy::jaeger_tracing;
use mongoproxy::dstaddr;
use mongoproxy::appconfig::{AppConfig};
use mongoproxy::tracker::{MongoStatsTracker};

use mongo_protocol::{MongoMessage};
use log::{info, warn, debug, error, LevelFilter};

type BufBytes = Result<bytes::BytesMut, io::Error>;

const JAEGER_ADDR: &str = "127.0.0.1:6831";
const ADMIN_PORT: &str = "9898";
const SERVICE_NAME: &str = "mongoproxy";

// Max number of bytes to read from the network
const READ_BUFFER_SIZE: usize = 16384;

// The largest message we can expect from MongoDb (oversize to be safe)
const MAX_MONGO_MESSAGE_SIZE: usize = 64*1024*1024;

// Max number of events the client and server message channels can take.
// We ought to be able to buffer the maximum MongoDb message there.
const MAX_CHANNEL_EVENTS: usize = MAX_MONGO_MESSAGE_SIZE / READ_BUFFER_SIZE;

lazy_static! {
    static ref MONGOPROXY_RUNTIME_INFO: CounterVec =
        register_counter_vec!(
            "mongoproxy_runtime_info",
            "Runtime information about Mongoproxy",
            &["version", "proxy", "service_name", "log_mongo_messages", "enable_jaeger"]).unwrap();

    static ref CONNECTION_COUNT_TOTAL: CounterVec =
        register_counter_vec!(
            "mongoproxy_client_connections_established_total",
            "Total number of client connections established",
            &["client"]).unwrap();

    static ref DISCONNECTION_COUNT_TOTAL: CounterVec =
        register_counter_vec!(
            "mongoproxy_client_disconnections_total",
            "Total number of client disconnections",
            &["client"]).unwrap();

    static ref CONNECTION_ERRORS_TOTAL: CounterVec =
        register_counter_vec!(
            "mongoproxy_client_connection_errors_total",
            "Total number of errors from handle_connections",
            &["client"]).unwrap();

    static ref SERVER_CONNECT_TIME_SECONDS: HistogramVec =
        register_histogram_vec!(
            "mongoproxy_server_connect_time_seconds",
            "Time it takes to look up and connect to a server",
            &["server_addr"]).unwrap();

    static ref TRACKER_CHANNEL_ERRORS_TOTAL: Counter =
        register_counter!(
            "mongoproxy_tracker_channel_errors_total",
            "Total number of errors from sending bytes to tracker channel").unwrap();
}

#[tokio::main]
async fn main() {
    let matches = App::new("mongoproxy")
        .version(crate_version!())
        .about("Proxies MongoDb requests to obtain metrics")
        .arg(Arg::with_name("proxy")
            .long("proxy")
            .value_name("local-port[:remote-host:remote-port]")
            .help("Port the proxy listens on (sidecar) and optionally\na target hostport (for static proxy)")
            .takes_value(true)
            .required(true))
        .arg(Arg::with_name("log_mongo_messages")
            .long("log-mongo-messages")
            .help("Log the contents of MongoDb messages (adds full BSON parsing)")
            .takes_value(false)
            .required(false))
        .arg(Arg::with_name("enable_jaeger")
            .long("enable-jaeger")
            .help("Enable distributed tracing with Jaeger")
            .takes_value(false)
            .required(false))
        .arg(Arg::with_name("jaeger_addr")
            .long("jaeger-addr")
            .value_name("Jaeger agent host:port")
            .help("Jaeger agent hostport to send traces to (compact thrift protocol)")
            .takes_value(true)
            .required(false))
        .arg(Arg::with_name("service_name")
            .long("service-name")
            .value_name("SERVICE_NAME")
            .help("Service name that will be used in Jaeger traces and metric labels")
            .takes_value(true))
        .arg(Arg::with_name("admin_port")
            .long("admin-port")
            .value_name("ADMIN_PORT")
            .help(&format!("Port the admin endpoints listens on (metrics and health). Default {}", ADMIN_PORT))
            .takes_value(true))
        .arg(Arg::with_name("use_tls")
            .long("use-tls")
            .help("Use TLS when to upstream TCP connection")
            .takes_value(false)
            .required(false))
        .arg(Arg::with_name("skip_host_verification")
            .long("skip-host-verification")
            .help("Skip verification of a TLS host")
            .takes_value(false)
            .required(false))
        .arg(Arg::with_name("debug")
            .long("debug")
            .help("Enable debug logging")
            .takes_value(false)
            .required(false))
        .get_matches();

    let admin_port = matches.value_of("admin_port").unwrap_or(ADMIN_PORT);
    let admin_addr = format!("0.0.0.0:{}", admin_port);
    let service_name = matches.value_of("service_name").unwrap_or(SERVICE_NAME);
    let log_mongo_messages = matches.occurrences_of("log_mongo_messages") > 0;
    let enable_jaeger = matches.occurrences_of("enable_jaeger") > 0;
    let jaeger_addr = lookup_address(matches.value_of("jaeger_addr").unwrap_or(JAEGER_ADDR)).unwrap();
    let use_tls = matches.occurrences_of("use_tls") > 0;
    let skip_host_verification = matches.occurrences_of("skip_host_verification") > 0;
    
    env_logger::builder().filter_level(
        if matches.occurrences_of("debug") > 0 { LevelFilter::Debug } else { LevelFilter::Info }
    ).init();

    let (writer, _guard) = tracing_appender::non_blocking(std::io::stdout());
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::TRACE)
        .with_writer(writer)
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(isatty::stdout_isatty())
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("setting default trace subscriber failed");

    info!("MongoProxy v{}", crate_version!());

    start_admin_listener(&admin_addr)
        .expect("failed to start admin listener");
    info!("Admin endpoint at http://{}", admin_addr);

    let proxy_spec = matches.value_of("proxy").unwrap();
    let (local_hostport, remote_hostport) = parse_proxy_addresses(proxy_spec).unwrap();

    let (tracer, _uninstall) = jaeger_tracing::init_tracer(enable_jaeger, &service_name, jaeger_addr);

    let app = AppConfig::new(
        tracer,
        log_mongo_messages,
        use_tls,
        skip_host_verification,
        service_name.to_string(),
    );

    MONGOPROXY_RUNTIME_INFO.with_label_values(&[
        crate_version!(),
        &proxy_spec,
        &service_name,
        if log_mongo_messages { "true" } else { "false" },
        if enable_jaeger { "true" } else { "false" } ],
    ).inc();

    run_accept_loop(local_hostport, remote_hostport, app).await;
}

// Accept connections in a loop and spawn a task to proxy them. If remote address is not explicitly
// specified attempt to proxy to the original destination obtained with SO_ORIGINAL_DST socket
// option.
//
// Never returns.
async fn run_accept_loop(local_addr: String, remote_addr: String, app: AppConfig)
{
    if remote_addr.is_empty() {
        info!("Proxying {} -> <original dst>", local_addr);
    } else {
        info!("Proxying {} -> {}", local_addr, remote_addr);
    }

    let listener = TcpListener::bind(&local_addr).await.unwrap();

    loop {
        match listener.accept().await {
            Ok((stream, peer_addr)) => {
                let client_ip_port = peer_addr.to_string();
                let client_addr = format_client_address(&peer_addr);

                let server_addr = if remote_addr.is_empty() {
                    if let Some(sockaddr) = dstaddr::orig_dst_addr(&stream) {
                        // This only assumes that NATd connections are received
                        // and thus always have a valid target address. We expect
                        // iptables rules to be in place to block direct access
                        // to the proxy port.
                        info!("Original destination address: {:?}", sockaddr);
                        sockaddr.to_string()
                    } else {
                        info!("Host not set and destination address not found: {}", client_addr);
                        // TODO: Increase a counter
                        continue;
                    }
                } else {
                    remote_addr.clone()
                };

                let app = app.clone();
                let server_ip_port = server_addr.clone();

                CONNECTION_COUNT_TOTAL.with_label_values(&[&client_addr.to_string()]).inc();

                let conn_handler = async move {
                    debug!("new connection from {}", client_addr);
                    match handle_connection(&server_addr, stream, app).await {
                        Ok(_) => {
                            debug!("{} closing connection.", client_addr);
                            DISCONNECTION_COUNT_TOTAL
                                .with_label_values(&[&client_addr.to_string()])
                                .inc();
                        },
                        Err(e) => {
                            warn!("{} connection error: {}", client_addr, e);
                            CONNECTION_ERRORS_TOTAL
                                .with_label_values(&[&client_addr.to_string()])
                                .inc();
                        },
                    };
                };

                tokio::spawn(
                    conn_handler.instrument(
                        tracing::info_span!("handle_connection",
                            client_addr = client_ip_port.as_str(),
                            server_addr = server_ip_port.as_str()))
                );
            },
            Err(e) => {
                debug!("accept: {:?}", e)
            },
        }
    }
}

// Open a connection to the server and start passing bytes between the client and the server. Also
// split the traffic to MongoDb protocol parser, so that we can get some stats out of this.
//
// The philosophy here is that we will not change any of the bytes that are passed between the
// client and the server. Instead we fork off a stream and send it to a separate tracker task,
// which then parses the messages and collects metrics from it. Should the tracker fail, the
// proxy still remains operational.
//

async fn handle_connection(server_addr: &str, client_stream: TcpStream, app: AppConfig)
    -> Result<(), io::Error>
{
    let use_tls = app.use_tls;
    let skip_host_verification = app.skip_host_verification;
    if use_tls {
        debug!("connecting to server with TLS: {}", server_addr);
    } else {
        debug!("connecting to server: {}", server_addr);
    }

    let timer = SERVER_CONNECT_TIME_SECONDS.with_label_values(&[server_addr]).start_timer();
    let server_addr = lookup_address(server_addr)?;
    let server_stream = TcpStream::connect(&server_addr).await?;
    timer.observe_duration();

    let client_addr = format_client_address(&client_stream.peer_addr()?);

    let log_mongo_messages = app.log_mongo_messages;
    let tracing_enabled = app.tracer.is_some();

    server_stream.set_nodelay(true)?;
    client_stream.set_nodelay(true)?;

    // Start the tracker to parse and track MongoDb messages from the input stream. This works by
    // having the proxy tasks send a copy of the bytes over a channel and process that channel
    // as a stream of bytes, extracting MongoDb messages and tracking the metrics from there.

    let (client_tx, client_rx): (mpsc::Sender<BufBytes>, mpsc::Receiver<BufBytes>) = mpsc::channel(MAX_CHANNEL_EVENTS);
    let (server_tx, server_rx): (mpsc::Sender<BufBytes>, mpsc::Receiver<BufBytes>) = mpsc::channel(MAX_CHANNEL_EVENTS);

    let signal_client = client_tx.clone();
    let signal_server = server_tx.clone();

    let tracker = MongoStatsTracker::new(
        &client_addr,
        &server_addr.to_string(),
        server_addr,
        app,
    );

    tokio::spawn(async move {
        track_mongo_messages(client_rx, server_rx, log_mongo_messages, tracing_enabled, tracker).await?;
        Ok::<(), io::Error>(())
    }.instrument(info_span!("tracker")));

    let (mut read_client, mut write_client) = client_stream.into_split();

    if use_tls {
        let connector = NativeTlsConnector::builder()
            .danger_accept_invalid_hostnames(skip_host_verification)
            .min_protocol_version(Some(Protocol::Tlsv12))
            .build()
            .unwrap();
        let result = TlsConnector::from(connector)
            .connect(&server_addr.to_string(), server_stream)
            .await;
        let mut tls_server_stream = match result {
            Ok(stream) => stream,
            Err(err) => panic!("{}", err),
        };

        let task = async {
            proxy_bytes_tls(
                &mut read_client,
                &mut write_client,
                &mut tls_server_stream,
                client_tx,
                server_tx,
            ).await?;
            Ok::<(), io::Error>(())
        }.instrument(info_span!("Sequential proxy"));

        match tokio::try_join!(task) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
            Err(e) => Err(e),
        }
    } else {
        let (mut read_server, mut write_server) = server_stream.into_split();

        let client_task = async {
            proxy_bytes(&mut read_client, &mut write_server, client_tx, signal_server).await?;
            Ok::<(), io::Error>(())
        }.instrument(info_span!("client proxy"));

        let server_task = async {
            proxy_bytes(&mut read_server, &mut write_client, server_tx, signal_client).await?;
            Ok::<(), io::Error>(())
        }.instrument(info_span!("server proxy"));

        match tokio::try_join!(client_task, server_task) {
            Ok(_) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(()),
            Err(e) => Err(e),
        }
    }
}

// Move bytes between sockets, forking the byte stream into a mpsc channel
// for processing. Another channel is used to notify the other tracker of
// failures.
async fn proxy_bytes(
    read_from: &mut OwnedReadHalf,
    write_to: &mut OwnedWriteHalf,
    tracker_channel: mpsc::Sender<BufBytes>,
    notify_channel: mpsc::Sender<BufBytes>,
) -> Result<(), io::Error>
{
    let mut tracker_ok = true;

    loop {
        let mut buf = bytes::BytesMut::with_capacity(READ_BUFFER_SIZE);
        let len = read_from.read_buf(&mut buf).await?;

        if len > 0 {
            write_to.write_all(&buf[0..len]).await?;

            if tracker_ok {
                if let Err(e) = tracker_channel.try_send(Ok(buf)) {
                    error!("error sending to tracker, stop: {}", e);
                    TRACKER_CHANNEL_ERRORS_TOTAL.inc();
                    tracker_ok = false;

                    // Let the other side know that we're closed.
                    let notification = io::Error::new(io::ErrorKind::UnexpectedEof, "notify channel close");
                    let _ = notify_channel.send(Err(notification)).await;
                }
            }
        } else {
            // EOF on read, return Err to signal try_join! to return
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
        }
    }
}

// Copy bytes from server to client and vice versa.
// TlsStream does not support splitting the server channel into receive and send.
// Hence, this is done sequentially. Copy bytes from client to server and server to client.
async fn proxy_bytes_tls(
    client_read_from: &mut OwnedReadHalf,
    client_write_to: &mut OwnedWriteHalf,
    server: &mut TlsStream<TcpStream>,
    client_tracker: mpsc::Sender<BufBytes>,
    server_tracker: mpsc::Sender<BufBytes>,
) -> Result<(), io::Error>
{
    let mut tracker_ok = true;

    loop {
        let mut client_buf = bytes::BytesMut::with_capacity(READ_BUFFER_SIZE);
        let client_len = client_read_from.read_buf(&mut client_buf).await?;

        if client_len > 0 {
            server.write_all(&client_buf[0..client_len]).await?;

            if tracker_ok {
                if let Err(e) = client_tracker.try_send(Ok(client_buf)) {
                    error!("error sending to tracker, stop: {}", e);
                    TRACKER_CHANNEL_ERRORS_TOTAL.inc();
                    tracker_ok = false;
                }
            }
        } else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
        }

        let mut server_buf = bytes::BytesMut::with_capacity(READ_BUFFER_SIZE);
        let server_len = server.read_buf(&mut server_buf).await?;

        if server_len > 0 {
            client_write_to.write_all(&server_buf[0..server_len]).await?;

            if tracker_ok {
                if let Err(e) = server_tracker.try_send(Ok(server_buf)) {
                    error!("error sending to tracker, stop: {}", e);
                    TRACKER_CHANNEL_ERRORS_TOTAL.inc();
                    tracker_ok = false;
                }
            }
        } else {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "EOF"));
        }
    }
}

// Process the mpsc channel as a byte stream, parsing MongoDb messages
// and sending them off to a tracker.
//
// XXX: We assume here that client always speaks first, followed by a response from the server,
// then the client goes again and then the server, and so on. This makes it easy to reason about
// things, and responses are never processes before the request. However this is prone to break
// when Mongo changes the protocol.
//
async fn track_mongo_messages(
    client_rx: mpsc::Receiver<BufBytes>,
    server_rx: mpsc::Receiver<BufBytes>,
    log_mongo_messages: bool,
    collect_tracing_data: bool,
    mut tracker: MongoStatsTracker,
) -> Result<(), io::Error>
{
    let mut client_stream = StreamReader::new(ReceiverStream::new(client_rx));
    let mut server_stream = StreamReader::new(ReceiverStream::new(server_rx));

    loop {
        match MongoMessage::from_reader(
            &mut client_stream,
            log_mongo_messages,
            collect_tracing_data)
            .instrument(info_span!("client"))
            .await
        {
            Ok((hdr, msg)) => tracker.track_client_request(&hdr, &msg),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => {
                error!("Client stream processing error: {}", e);
                return Err(e);
            }
        }

        match MongoMessage::from_reader(
            &mut server_stream,
            log_mongo_messages,
            collect_tracing_data)
            .instrument(info_span!("server"))
            .await
        {
            Ok((hdr, msg)) => tracker.track_server_response(hdr, msg),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => {
                error!("Server stream processing failed: {}", e);
                return Err(e);
            }
        }
    }
}

fn lookup_address(addr: &str) -> std::io::Result<SocketAddr> {
    if let Some(sockaddr) = addr.to_socket_addrs()?.next() {
        debug!("{} resolves to {}", addr, sockaddr);
        return Ok(sockaddr);
    }
    Err(io::Error::new(io::ErrorKind::AddrNotAvailable, "no usable address found"))
}

// Return the peer address of the stream without the :port
fn format_client_address(sockaddr: &SocketAddr) -> String {
    let mut addr_str = sockaddr.to_string();
    if let Some(pos) = addr_str.find(':') {
        let _ = addr_str.split_off(pos);
    }
    addr_str
}

// Parse the local and remote address pair from provided proxy definition
fn parse_proxy_addresses(proxy_def: &str) -> Result<(String,String), io::Error> {
    if let Some(pos) = proxy_def.find(':') {
        let (local_port, remote_hostport) = proxy_def.split_at(pos);
        let local_addr = format!("0.0.0.0:{}", local_port);

        Ok((local_addr, remote_hostport[1..].to_string()))
    } else {
        Ok((format!("0.0.0.0:{}", proxy_def), String::from("")))
    }
}

async fn serve_admin_req(req: Request<Body>) -> Result<Response<Body>, Infallible> {
    let mut response = Response::new(Body::empty());

    match (req.method(), req.uri().path()) {
        (&Method::GET, "/") => {
            *response.body_mut() = Body::from("/");
        },
        (&Method::GET, "/health") => {
            *response.body_mut() = Body::from("OK");
        },
        (&Method::GET, "/metrics") => {
            let encoder = TextEncoder::new();
            let metric_families = prometheus::gather();
            let mut buffer = vec![];
            encoder.encode(&metric_families, &mut buffer).unwrap();

            *response.body_mut() = Body::from(buffer);
        },
        _ => {
            *response.status_mut() = StatusCode::NOT_FOUND;
        },
    };

    Ok(response)
}

pub fn start_admin_listener(endpoint: &str) -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = endpoint.to_string();
    let addr: SocketAddr = endpoint.parse()?;

    tokio::spawn(async move {
        Server::bind(&addr)
            .serve(make_service_fn(|_conn| async {
                Ok::<_, Infallible>(service_fn(serve_admin_req))
            }))
            .await?;
        Ok::<(), hyper::Error>(())
    });

    Ok(())
}
