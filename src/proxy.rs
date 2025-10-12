// ===========================================
// Imports
// ===========================================
use byteorder::{BigEndian, ReadBytesExt};
use lazy_static::lazy_static;
use log::{debug, error, info};
use proxy_protocol::{
    version2::{ProxyAddresses, ProxyCommand, ProxyTransportProtocol},
    ProxyHeader,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Cursor, Read};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;
// Additional dependency for concurrent maps
use dashmap::DashMap;

use crate::config_loader::{
    resolve, RedirectionConfig, BIND_ADDRESS, DEBUG, LIMBO_ENABLED, LIMBO_HOLD_DURATION_MS,
    LIMBO_VERIFICATION_TIMEOUT_MS,
};
use crate::limbo::{self, HandshakeCapture};
use crate::{forwarding, send_ntfy_notification, target_pinger};
use std::sync::Arc;

// ===========================================
// Helper Macros for Conditional Logging
// ===========================================
macro_rules! log_error {
    ($($arg:tt)*) => {{
         if DEBUG.load(Ordering::Relaxed) {
             error!($($arg)*);
         }
    }};
}

macro_rules! log_info {
    ($($arg:tt)*) => {{
         if DEBUG.load(Ordering::Relaxed) {
             info!($($arg)*);
         }
    }};
}

macro_rules! log_debug {
    ($($arg:tt)*) => {{
         if DEBUG.load(Ordering::Relaxed) {
             debug!($($arg)*);
         }
    }};
}

// ===========================================
// Global Concurrent Maps using DashMap
// ===========================================
// IP status: 0=blocked,1=regular,2=pinged,3=played>2m,4=played>6m
#[derive(Clone, Copy, Serialize, Deserialize)]
struct IpInfo {
    status: u8,
    expiry: Option<u64>,
}

lazy_static! {
    // Combined IP status map
    static ref IP_STATUS: DashMap<IpAddr, IpInfo> = DashMap::new();
    // (domain, source_ip) -> (timestamp_in_seconds, ping_count_this_second)
    static ref DOMAIN_SRC_PING_COUNT: DashMap<(String, IpAddr), (u64, usize)> = DashMap::new();
    // These per-(domain, IP) maps are pruned periodically; without cleanup they
    // could theoretically grow with every reachable IPv4/domain pair.
    // (domain, source_ip) -> (timestamp_in_seconds,
    //                        regular_conn_count,
    //                        pinged_conn_count,
    //                        trusted_2m_conn_count,
    //                        trusted_6m_conn_count)
    static ref DOMAIN_SRC_CONN_COUNT: DashMap<(String, IpAddr), (u64, usize, usize, usize, usize)> = DashMap::new();
    // Active connections we track with their start time to decide trust promotion
    static ref ACTIVE_CONNECTIONS: DashMap<SocketAddr, Instant> = DashMap::new();
}

const CONNECT_TIMEOUT_SECS: u64 = 5;

// SQLite database file to persist IP status between restarts
const IP_STATUS_DB: &str = "ip_status.sqlite";

fn save_ip_status() -> io::Result<()> {
    let mut conn =
        Connection::open(IP_STATUS_DB).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ip_status (ip TEXT PRIMARY KEY, status INTEGER NOT NULL, expiry INTEGER)",
        [],
    )
    .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    let tx = conn
        .transaction()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    tx.execute("DELETE FROM ip_status", [])
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    for kv in IP_STATUS.iter() {
        let ip_str = kv.key().to_string();
        let info = *kv.value();
        tx.execute(
            "INSERT OR REPLACE INTO ip_status (ip, status, expiry) VALUES (?1, ?2, ?3)",
            params![ip_str, info.status as i64, info.expiry.map(|e| e as i64)],
        )
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    }

    tx.commit()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    Ok(())
}

fn load_ip_status() -> usize {
    let conn = match Connection::open(IP_STATUS_DB) {
        Ok(c) => c,
        Err(e) => {
            log_error!("Failed to open {}: {}", IP_STATUS_DB, e);
            return 0;
        }
    };

    if let Err(e) = conn.execute(
        "CREATE TABLE IF NOT EXISTS ip_status (ip TEXT PRIMARY KEY, status INTEGER NOT NULL, expiry INTEGER)",
        [],
    ) {
        log_error!("Failed to create table {}: {}", IP_STATUS_DB, e);
        return 0;
    }

    let mut stmt = match conn.prepare("SELECT ip, status, expiry FROM ip_status") {
        Ok(s) => s,
        Err(e) => {
            log_error!("Failed to query {}: {}", IP_STATUS_DB, e);
            return 0;
        }
    };
    let rows = match stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2)?,
        ))
    }) {
        Ok(r) => r,
        Err(e) => {
            log_error!("Failed to read {}: {}", IP_STATUS_DB, e);
            return 0;
        }
    };

    let mut count = 0;
    for row in rows {
        if let Ok((ip_str, status, expiry_opt)) = row {
            if let Ok(ip) = ip_str.parse::<IpAddr>() {
                IP_STATUS.insert(
                    ip,
                    IpInfo {
                        status: status as u8,
                        expiry: expiry_opt.map(|e| e as u64),
                    },
                );
                count += 1;
            }
        }
    }
    count
}

fn purge_blocked_ips() {
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    IP_STATUS.retain(|_, info| {
        if info.status == 0 {
            if let Some(expiry) = info.expiry {
                expiry > now_secs
            } else {
                false
            }
        } else {
            true
        }
    });
}

// ===========================================
// Main Proxy Struct & Entry Point
// ===========================================
pub struct TcpProxy {
    pub forward_task: tokio::task::JoinHandle<()>,
}

impl TcpProxy {
    pub async fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let loaded = load_ip_status();
        log_info!("Loaded {} IP(s) from {}", loaded, IP_STATUS_DB);
        purge_blocked_ips();

        thread::spawn(|| loop {
            thread::sleep(Duration::from_secs(300));
            purge_blocked_ips();
        });

        // Periodically save IP status in a background thread
        thread::spawn(|| loop {
            thread::sleep(Duration::from_secs(60));
            if let Err(e) = save_ip_status() {
                log_error!("Failed to save IP status: {}", e);
            }
        });

        // Start cleanup thread for rate-limit maps and connection promotions
        thread::spawn(|| loop {
            let now_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs();
            // Keep entries from the current and previous second to avoid
            // interfering with in-flight operations that span a second boundary.
            DOMAIN_SRC_PING_COUNT.retain(|_, v| v.0 + 1 >= now_secs);
            DOMAIN_SRC_CONN_COUNT.retain(|_, v| v.0 + 1 >= now_secs);

            let now_inst = Instant::now();
            for kv in ACTIVE_CONNECTIONS.iter() {
                let dur = now_inst.duration_since(*kv.value());
                let ip = kv.key().ip();
                if dur >= Duration::from_secs(360) {
                    IP_STATUS.insert(
                        ip,
                        IpInfo {
                            status: 4,
                            expiry: None,
                        },
                    );
                } else if dur >= Duration::from_secs(120) {
                    IP_STATUS
                        .entry(ip)
                        .and_modify(|info| {
                            if info.status < 3 {
                                info.status = 3;
                                info.expiry = None;
                            }
                        })
                        .or_insert(IpInfo {
                            status: 3,
                            expiry: None,
                        });
                }
            }

            thread::sleep(Duration::from_secs(1));
        });

        // Start packet rate-limit cleanup thread
        forwarding::start_packet_cleanup_thread();

        let bind_addr = *BIND_ADDRESS.lock().unwrap();
        let listener = TcpListener::bind(bind_addr).await?;

        let forward_task = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        if let Err(e) = stream.set_nodelay(true) {
                            log_error!("Failed to disable Nagle: {}", e);
                        }
                        tokio::spawn(handle_client(stream));
                    }
                    Err(e) => log_error!("Accept error: {}", e),
                }
            }
        });

        Ok(Self { forward_task })
    }
}

// ===========================================
// Connection Handling & Main Flow
// ===========================================
async fn handle_client(mut client_stream: TcpStream) {
    let src_addr = match client_stream.peer_addr() {
        Ok(addr) => addr,
        Err(e) => {
            log_error!("Failed to get peer address: {}", e);
            let _ = client_stream.shutdown().await;
            return;
        }
    };
    let src_ip = src_addr.ip();
    if is_ip_blocked(&src_ip) {
        let _ = client_stream.shutdown().await;
        return;
    }

    let mut buf = vec![0; 1024];
    let n = match timeout(Duration::from_secs(5), client_stream.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => n,
        Ok(Ok(_)) => return,
        Ok(Err(e)) => {
            log_error!("Read error from {}: {}", src_ip, e);
            let _ = client_stream.shutdown().await;
            return;
        }
        Err(_) => {
            log_error!("Handshake read timeout from {}", src_ip);
            let _ = client_stream.shutdown().await;
            return;
        }
    };
    buf.truncate(n);

    let handshake_capture =
        match limbo::capture_handshake(&mut client_stream, buf, Duration::from_secs(5)).await {
            Ok(capture) => capture,
            Err(err) => {
                log_error!(
                    "Failed to capture complete handshake from {}: {}",
                    src_ip,
                    err
                );
                let _ = client_stream.shutdown().await;
                return;
            }
        };

    let (server_address, next_state, client_protocol) = match decode_handshake_packet_ext(
        handshake_capture.buffer(),
    ) {
        Ok(res) => res,
        Err(e) => {
            let err_msg = format!(
                "Handshake decoding error from {}: {}. Mitigating potential attack, blocking ip for 300 secs",
                src_ip, e
            );
            log_error!("{}", err_msg);
            send_ntfy_notification(&err_msg);
            block_ip(src_ip);
            let _ = client_stream.shutdown().await;
            return;
        }
    };

    match next_state {
        1 => {
            if let Some(cfg) = resolve(&server_address) {
                if !check_ping_limit(&server_address, src_ip, &cfg) {
                    let err_msg = format!(
                        "Too many incoming pings to domain '{}' from IP '{}' mitigating DoS attack, blocking ip for 300 secs",
                        server_address, src_ip
                    );
                    log_error!("{}", err_msg);
                    send_ntfy_notification(&err_msg);
                    block_ip(src_ip);
                    let _ = client_stream.shutdown().await;
                    return;
                }
            }
            record_ping(src_ip);
            let response = if let Some(status_json) = target_pinger::STATUS_CACHE
                .get(&server_address)
                .map(|v| v.value().clone())
            {
                send_status_response(&mut client_stream, &status_json, client_protocol).await
            } else {
                send_fallback_status_response(&mut client_stream).await
            };
            if let Err(e) = response {
                log_error!("Error sending status response: {}", e);
            }
        }
        2 => {
            if let Some(redirection_cfg) = resolve(&server_address) {
                let redirection_cfg = Arc::new(redirection_cfg);
                if check_connection_limit(&server_address, src_ip, &redirection_cfg) {
                    let initial_buffer = if LIMBO_ENABLED.load(Ordering::Relaxed) {
                        let handshake_timeout = Duration::from_millis(
                            LIMBO_VERIFICATION_TIMEOUT_MS.load(Ordering::Relaxed),
                        );
                        let hold_duration =
                            Duration::from_millis(LIMBO_HOLD_DURATION_MS.load(Ordering::Relaxed));
                        match verify_with_limbo(
                            &mut client_stream,
                            handshake_capture,
                            client_protocol,
                            handshake_timeout,
                            hold_duration,
                            &server_address,
                            src_ip,
                        )
                        .await
                        {
                            Ok(buffer) => buffer,
                            Err(err) => {
                                log_error!(
                                    "Limbo verification failed for {} from {}: {}",
                                    server_address,
                                    src_ip,
                                    err
                                );
                                let _ = client_stream.shutdown().await;
                                return;
                            }
                        }
                    } else {
                        log_debug!(
                            "Limbo verification disabled, connecting {} from {} directly to target",
                            server_address,
                            src_ip
                        );
                        handshake_capture.into_buffer()
                    };
                    let proxy_to = SocketAddr::new(redirection_cfg.ip.into(), redirection_cfg.port);
                    match timeout(
                        Duration::from_secs(CONNECT_TIMEOUT_SECS),
                        TcpStream::connect(proxy_to),
                    )
                    .await
                    {
                        Ok(Ok(mut target_stream)) => {
                            if let Err(e) = target_stream.set_nodelay(true) {
                                log_error!("Failed to disable Nagle on target: {}", e);
                            }
                            if let (Ok(src_addr), Ok(dst_addr)) =
                                (client_stream.peer_addr(), target_stream.peer_addr())
                            {
                                let new_buf = encapsulate_with_proxy_protocol(
                                    src_addr,
                                    dst_addr,
                                    &initial_buffer,
                                );
                                send_initial_buffer_to_target(&new_buf, &mut target_stream).await;
                            }
                            let (client_read, client_write) = client_stream.into_split();
                            let (target_read, target_write) = target_stream.into_split();
                            ACTIVE_CONNECTIONS.insert(src_addr, Instant::now());
                            let domain_clone = server_address.clone();
                            let cfg_clone1 = redirection_cfg.clone();
                            let cfg_clone2 = redirection_cfg.clone();
                            tokio::spawn(async move {
                                forwarding::forward_loop(
                                    client_read,
                                    target_write,
                                    server_address,
                                    cfg_clone1,
                                    src_ip,
                                    src_addr,
                                    true,
                                    "client->target",
                                )
                                .await;
                            });
                            tokio::spawn(async move {
                                forwarding::forward_loop(
                                    target_read,
                                    client_write,
                                    domain_clone,
                                    cfg_clone2,
                                    src_ip,
                                    src_addr,
                                    false,
                                    "target->client",
                                )
                                .await;
                            });
                        }
                        Ok(Err(e)) => {
                            log_error!(
                                "Error connecting to target server for {}: {}",
                                server_address,
                                e
                            );
                            let _ = client_stream.shutdown().await;
                        }
                        Err(_) => {
                            log_error!(
                                "Timeout connecting to target server for {}",
                                server_address
                            );
                            let _ = client_stream.shutdown().await;
                        }
                    }
                } else {
                    let err_msg = format!(
                        "Too many incoming connections to domain '{}' from IP '{}' mitigating DoS attack, blocking ip for 300 secs",
                        server_address, src_ip
                    );
                    log_error!("{}", err_msg);
                    send_ntfy_notification(&err_msg);
                    block_ip(src_ip);
                    let _ = client_stream.shutdown().await;
                    return;
                }
            } else {
                log_info!(
                    "No redirection configuration for domain {}, sending fallback response",
                    server_address
                );
                if let Err(e) = send_fallback_status_response(&mut client_stream).await {
                    log_error!("Error sending fallback response: {}", e);
                }
            }
        }
        other => {
            log_error!(
                "Unknown next_state ({}) from {} for domain {}",
                other,
                src_ip,
                server_address
            );
            let _ = client_stream.shutdown().await;
        }
    }
}

// ===========================================
// Helper Functions: Handshake Decoding
// ===========================================
fn decode_handshake_packet_ext(buffer: &[u8]) -> io::Result<(String, u32, u32)> {
    let mut cursor = Cursor::new(buffer);
    let _total_len = read_varint(&mut cursor)?; // packet length
    let _packet_id = read_varint(&mut cursor)?; // handshake packet id
    let protocol_version = read_varint(&mut cursor)?; // protocol version
    let server_address = read_string(&mut cursor)?.to_ascii_lowercase(); // domain
    let _server_port = ReadBytesExt::read_u16::<BigEndian>(&mut cursor)?;
    let next_state = read_varint(&mut cursor)?; // 1=status, 2=login
    Ok((server_address, next_state, protocol_version))
}

fn read_string(cursor: &mut Cursor<&[u8]>) -> io::Result<String> {
    let len = read_varint(cursor)? as usize;
    const MAX_STRING_LEN: usize = 255;
    if len > MAX_STRING_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "String too long",
        ));
    }
    let mut buf = vec![0; len];
    std::io::Read::read_exact(cursor, &mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn read_varint<R: Read>(reader: &mut R) -> io::Result<u32> {
    let mut num_read = 0;
    let mut result = 0;
    loop {
        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf)?;
        let byte = buf[0];
        result |= ((byte & 0x7F) as u32) << (7 * num_read);
        num_read += 1;
        if num_read > 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VarInt too long",
            ));
        }
        if byte & 0x80 == 0 {
            break;
        }
    }
    Ok(result)
}

// ===========================================
// Helper Functions: Proxy Protocol V2
// ===========================================
fn encapsulate_with_proxy_protocol(src: SocketAddr, dst: SocketAddr, data: &[u8]) -> Vec<u8> {
    let header = encode_proxy_protocol_v2_header(src, dst);
    let mut buf = Vec::with_capacity(header.len() + data.len());
    buf.extend_from_slice(&header);
    buf.extend_from_slice(data);
    buf
}

fn encode_proxy_protocol_v2_header(src: SocketAddr, dst: SocketAddr) -> Vec<u8> {
    let proxy_addr = match (src, dst) {
        (SocketAddr::V4(s), SocketAddr::V4(d)) => ProxyAddresses::Ipv4 {
            source: s,
            destination: d,
        },
        _ => unreachable!(),
    };
    proxy_protocol::encode(ProxyHeader::Version2 {
        command: ProxyCommand::Proxy,
        transport_protocol: ProxyTransportProtocol::Stream,
        addresses: proxy_addr,
    })
    .unwrap()
    .to_vec()
}

// ===========================================
// Helper Functions: Rate Limiting
// ===========================================
fn check_ping_limit(domain: &str, src_ip: IpAddr, cfg: &RedirectionConfig) -> bool {
    let limit = cfg.max_ping_response_per_second;
    if limit == 0 {
        return true; // unlimited
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let key = (domain.to_string(), src_ip);
    let mut entry = DOMAIN_SRC_PING_COUNT.entry(key).or_insert((now, 0));
    if entry.0 == now {
        if entry.1 >= limit {
            return false;
        }
        entry.1 += 1;
    } else {
        *entry = (now, 1);
    }
    true
}

/// Check or increment the per‑domain + per‑source connection count.
/// Returns `true` if allowed, or `false` if the limit is exceeded.
fn check_connection_limit(domain: &str, src_ip: IpAddr, cfg: &RedirectionConfig) -> bool {
    let limit = cfg.max_connections_per_second;
    if limit == 0 {
        return true; // unlimited
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let key = (domain.to_string(), src_ip);
    let status = IP_STATUS.get(&src_ip).map(|info| info.status).unwrap_or(1);
    let mut entry = DOMAIN_SRC_CONN_COUNT
        .entry(key)
        .or_insert((now, 0, 0, 0, 0));
    if entry.0 != now {
        *entry = (now, 0, 0, 0, 0);
    }
    match status {
        4 => {
            if entry.4 >= limit {
                return false;
            }
            entry.4 += 1;
        }
        3 => {
            let allowed = limit.saturating_sub(entry.4);
            if entry.3 >= allowed {
                return false;
            }
            entry.3 += 1;
        }
        2 => {
            let allowed = limit.saturating_sub(entry.4 + entry.3);
            if entry.2 >= allowed {
                return false;
            }
            entry.2 += 1;
        }
        _ => {
            let allowed = limit.saturating_sub(entry.4 + entry.3 + entry.2);
            if entry.1 >= allowed {
                return false;
            }
            entry.1 += 1;
        }
    }
    true
}

pub(crate) fn finish_connection(addr: SocketAddr) {
    ACTIVE_CONNECTIONS.remove(&addr);
}

// ===========================================
// Helper Functions: Blacklist ips
// ===========================================
// Check if the IP is currently blocked based on status and expiry
fn is_ip_blocked(ip: &IpAddr) -> bool {
    if let Some(info) = IP_STATUS.get(ip) {
        if info.status == 0 {
            if let Some(expiry) = info.expiry {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                if now < expiry {
                    return true;
                }
            }
            IP_STATUS.remove(ip);
        }
    }
    false
}

pub(crate) fn block_ip(ip: IpAddr) {
    let expiry = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 300;
    IP_STATUS.insert(
        ip,
        IpInfo {
            status: 0,
            expiry: Some(expiry),
        },
    );
}

fn record_ping(ip: IpAddr) {
    IP_STATUS
        .entry(ip)
        .and_modify(|info| {
            if info.status != 0 && info.status < 2 {
                info.status = 2;
                info.expiry = None;
            }
        })
        .or_insert(IpInfo {
            status: 2,
            expiry: None,
        });
}

// ===========================================
// Limbo Verification Helpers
// ===========================================
async fn verify_with_limbo(
    client_stream: &mut TcpStream,
    handshake_capture: HandshakeCapture,
    client_protocol: u32,
    handshake_timeout: Duration,
    hold_duration: Duration,
    domain: &str,
    src_ip: IpAddr,
) -> io::Result<Vec<u8>> {
    log_debug!(
        "Starting in-process limbo verification for {} from {} (hold {} ms)",
        domain,
        src_ip,
        hold_duration.as_millis()
    );

    match limbo::verify_client_stream(
        client_stream,
        handshake_capture,
        client_protocol,
        handshake_timeout,
        hold_duration,
    )
    .await
    {
        Ok(buffer) => {
            log_debug!(
                "Limbo verification completed for {} from {}",
                domain,
                src_ip
            );
            Ok(buffer)
        }
        Err(err) => Err(err),
    }
}

// ===========================================
// Helper Functions: Status / Ping Responses
// ===========================================
async fn send_status_response(
    stream: &mut TcpStream,
    status_json: &str,
    client_protocol: u32,
) -> io::Result<()> {
    let adjusted_json = match adjust_status_response_for_client(status_json, client_protocol) {
        Ok(s) => s,
        Err(e) => {
            log_error!("Failed to adjust status response: {}", e);
            status_json.to_owned()
        }
    };

    let mut resp = Vec::new();
    resp.push(0x00);
    write_varint(adjusted_json.len() as u32, &mut resp);
    resp.extend_from_slice(adjusted_json.as_bytes());

    let mut packet = Vec::new();
    write_varint(resp.len() as u32, &mut packet);
    packet.extend_from_slice(&resp);

    stream.write_all(&packet).await?;
    stream.flush().await?;
    handle_ping(stream).await
}

async fn send_fallback_status_response(stream: &mut TcpStream) -> io::Result<()> {
    let protocol_version = 754;
    let json = format!(
        r#"{{
  "version": {{"name": "1.21.1", "protocol": {}}},
  "players": {{"max": 0, "online": 0, "sample": []}},
  "description": {{"text": "Unknown domain / server not found!"}}
}}"#,
        protocol_version
    );

    let mut resp = Vec::new();
    resp.push(0x00);
    write_varint(json.len() as u32, &mut resp);
    resp.extend_from_slice(json.as_bytes());

    let mut packet = Vec::new();
    write_varint(resp.len() as u32, &mut packet);
    packet.extend_from_slice(&resp);

    stream.write_all(&packet).await?;
    stream.flush().await?;
    handle_ping(stream).await
}

fn adjust_status_response_for_client(
    status_json: &str,
    client_protocol: u32,
) -> Result<String, serde_json::Error> {
    let mut value: Value = serde_json::from_str(status_json)?;
    if let Some(version) = value.get_mut("version") {
        version["protocol"] = serde_json::json!(client_protocol);
    }
    serde_json::to_string(&value)
}

async fn handle_ping(stream: &mut TcpStream) -> io::Result<()> {
    loop {
        let _len = read_varint_async(stream).await?;
        let packet_id = read_varint_async(stream).await?;
        match packet_id {
            0x00 => {
                log_debug!("Received additional status request instead of ping");
            }
            0x01 => {
                let mut payload = [0u8; 8];
                stream.read_exact(&mut payload).await?;
                let mut pong = Vec::new();
                pong.push(0x01);
                pong.extend_from_slice(&payload);

                let mut pong_packet = Vec::new();
                write_varint(pong.len() as u32, &mut pong_packet);
                pong_packet.extend_from_slice(&pong);
                stream.write_all(&pong_packet).await?;
                stream.flush().await?;
                return Ok(());
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Unexpected packet in ping: 0x{:02X}", packet_id),
                ));
            }
        }
    }
}

// ===========================================
// Utility Functions
// ===========================================
fn write_varint(mut value: u32, buf: &mut Vec<u8>) {
    while value > 0x7F {
        buf.push(((value & 0x7F) as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

async fn send_initial_buffer_to_target(initial: &[u8], sender: &mut TcpStream) {
    if sender.write_all(initial).await.is_err() || sender.flush().await.is_err() {
        log_error!("Failed to send initial buffer to target");
    }
}

async fn read_varint_async(stream: &mut TcpStream) -> io::Result<u32> {
    let mut num_read = 0;
    let mut result = 0;
    loop {
        let mut buf = [0u8; 1];
        stream.read_exact(&mut buf).await?;
        let byte = buf[0];
        result |= ((byte & 0x7F) as u32) << (7 * num_read);
        num_read += 1;
        if num_read > 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VarInt too long",
            ));
        }
        if byte & 0x80 == 0 {
            break;
        }
    }
    Ok(result)
}
