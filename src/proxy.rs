// ===========================================
// Imports
// ===========================================
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use byteorder::{BigEndian, ReadBytesExt};
use proxy_protocol::{
    version2::{ProxyAddresses, ProxyCommand, ProxyTransportProtocol},
    ProxyHeader,
};
use rayon::ThreadPoolBuilder;
use serde_json::Value;
use log::{error, info, debug};
use lazy_static::lazy_static;
use std::process::Command;
use std::sync::Mutex;
// Additional dependency for concurrent maps
use dashmap::DashMap;

use crate::config_loader::{
    resolve, RedirectionConfig, PROXY_THREADS, BIND_ADDRESS, DEBUG,
};
use crate::{forwarding, send_ntfy_notification, target_pinger};

// ===========================================
// Helper Macros for Conditional Logging
// ===========================================
macro_rules! log_error {
    ($($arg:tt)*) => {{
         if *DEBUG.lock().unwrap() {
             error!($($arg)*);
         }
    }};
}

macro_rules! log_info {
    ($($arg:tt)*) => {{
         if *DEBUG.lock().unwrap() {
             info!($($arg)*);
         }
    }};
}

macro_rules! log_debug {
    ($($arg:tt)*) => {{
         if *DEBUG.lock().unwrap() {
             debug!($($arg)*);
         }
    }};
}

// ===========================================
// Global Concurrent Maps using DashMap
// ===========================================
lazy_static! {
    // Blocked IPs map: IpAddr -> expiry timestamp
    pub static ref BLOCKED_IPS: DashMap<IpAddr, u64> = DashMap::new();
    // (domain, source_ip) -> (timestamp_in_seconds, ping_count_this_second)
    static ref DOMAIN_SRC_PING_COUNT: DashMap<(String, IpAddr), (u64, usize)> = DashMap::new();
    // (domain, source_ip) -> (timestamp_in_seconds, connection_count_this_second)
    static ref DOMAIN_SRC_CONN_COUNT: DashMap<(String, IpAddr), (u64, usize)> = DashMap::new();
    // Global last notification time for rate-limiting ntfy messages (in seconds)
    static ref LAST_NTFY_TIME: Mutex<u64> = Mutex::new(0);
    // A dedicated thread pool for bidirectional forwarding (size adjustable)
    static ref FORWARDING_POOL: rayon::ThreadPool = ThreadPoolBuilder::new().num_threads(50).build().unwrap();
}

// ===========================================
// Main Proxy Struct & Entry Point
// ===========================================
pub struct TcpProxy {
    pub forward_thread: thread::JoinHandle<()>,
}

impl TcpProxy {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Start a cleanup thread that runs every 10 seconds to remove expired IPs.
        thread::spawn(|| {
            loop {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                BLOCKED_IPS.retain(|_, &mut expiry| expiry > now);
                thread::sleep(Duration::from_secs(1));
            }
        });

        let bind_addr = *BIND_ADDRESS.lock().unwrap();
        let listener = TcpListener::bind(bind_addr)?;

        let num_threads = *PROXY_THREADS.lock().unwrap();
        let pool = ThreadPoolBuilder::new().num_threads(num_threads).build()?;

        let forward_thread = thread::spawn(move || {
            for stream_result in listener.incoming() {
                match stream_result {
                    Ok(mut stream) => {
                        // Disable Nagle for lower latency
                        if let Err(e) = stream.set_nodelay(true) {
                            log_error!("Failed to disable Nagle: {}", e);
                        }
                        // Set a read timeout to mitigate slowloris-style attacks.
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));

                        let src_ip = match stream.peer_addr() {
                            Ok(addr) => addr.ip(),
                            Err(e) => {
                                log_error!("Failed to get peer address: {}", e);
                                let _ = stream.shutdown(Shutdown::Both);
                                continue;
                            }
                        };
                        if is_ip_blocked(&src_ip) {
                            let _ = stream.shutdown(Shutdown::Both);
                            continue;
                        }
                        pool.spawn(|| handle_client(stream));
                    }
                    Err(e) => log_error!("Accept error: {}", e),
                }
            }
        });

        Ok(Self { forward_thread })
    }
}

// ===========================================
// Connection Handling & Main Flow
// ===========================================
fn handle_client(mut client_stream: TcpStream) {
    // 1. Get the source IP and check if it is blocked.
    let src_ip = match client_stream.peer_addr() {
        Ok(addr) => addr.ip(),
        Err(e) => {
            log_error!("Failed to get peer address: {}", e);
            let _ = client_stream.shutdown(Shutdown::Both);
            return;
        }
    };
    if is_ip_blocked(&src_ip) {
        let _ = client_stream.shutdown(Shutdown::Both);
        return;
    }

    // 2. Read handshake data into a fixed-size buffer.
    let mut buf = vec![0; 1024];
    let n = match client_stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        Ok(_) => return, // EOF reached
        Err(e) => {
            log_error!("Read error from {}: {}", src_ip, e);
            let _ = client_stream.shutdown(Shutdown::Both);
            return;
        }
    };
    buf.truncate(n);

    // 3. Decode the handshake packet; block the IP if decoding fails.
    let (server_address, next_state, client_protocol) = match decode_handshake_packet_ext(&buf) {
        Ok(res) => res,
        Err(e) => {
            let err_msg = format!(
                "Handshake decoding error from {}: {}. Mitigating potential attack, blocking ip for 300 secs",
                src_ip, e
            );
            log_error!("{}", err_msg);
            send_ntfy_notification(&err_msg);
            block_ip(src_ip);
            let _ = client_stream.shutdown(Shutdown::Both);
            return;
        }
    };

    // 4. Process based on the request type: status (ping) or login.
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
                    let _ = client_stream.shutdown(Shutdown::Both);
                    return;
                }
            }
            let response = if let Some(status_json) =
                target_pinger::STATUS_CACHE.lock().unwrap().get(&server_address).cloned()
            {
                send_status_response(&mut client_stream, &status_json, client_protocol)
            } else {
                send_fallback_status_response(&mut client_stream)
            };
            if let Err(e) = response {
                log_error!("Error sending status response: {}", e);
            }
        }
        2 => {
            if let Some(redirection_cfg) = resolve(&server_address) {
                if check_connection_limit(&server_address, src_ip, &redirection_cfg) {
                    let proxy_to = SocketAddr::new(redirection_cfg.ip.into(), redirection_cfg.port);
                    match TcpStream::connect(proxy_to) {
                        Ok(mut target_stream) => {
                            if let Err(e) = target_stream.set_nodelay(true) {
                                log_error!("Failed to disable Nagle on target: {}", e);
                            }
                            if let (Ok(src_addr), Ok(dst_addr)) = (client_stream.peer_addr(), target_stream.peer_addr()) {
                                let new_buf = encapsulate_with_proxy_protocol(src_addr, dst_addr, &buf);
                                send_initial_buffer_to_target(&new_buf, &mut target_stream);
                            }
                            let target_clone = match target_stream.try_clone() {
                                Ok(tc) => tc,
                                Err(e) => {
                                    log_error!("Error cloning target stream: {}", e);
                                    return;
                                }
                            };
                            let client_clone = match client_stream.try_clone() {
                                Ok(cc) => cc,
                                Err(e) => {
                                    log_error!("Error cloning client stream: {}", e);
                                    return;
                                }
                            };
                            let domain_clone = server_address.clone();
                            FORWARDING_POOL.spawn(move || {
                                forwarding::forward_loop(client_clone, target_stream, server_address, src_ip, true, "client->target")
                            });
                            FORWARDING_POOL.spawn(move || {
                                forwarding::forward_loop(target_clone, client_stream, domain_clone, src_ip, false, "target->client")
                            });
                        }
                        Err(e) => {
                            log_error!("Error connecting to target server for {}: {}", server_address, e);
                            let _ = client_stream.shutdown(Shutdown::Both);
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
                    let _ = client_stream.shutdown(Shutdown::Both);
                    return;
                }
            } else {
                log_info!("No redirection configuration for domain {}, sending fallback response", server_address);
                if let Err(e) = send_fallback_status_response(&mut client_stream) {
                    log_error!("Error sending fallback response: {}", e);
                }
            }
        }
        other => {
            log_error!("Unknown next_state ({}) from {} for domain {}", other, src_ip, server_address);
            let _ = client_stream.shutdown(Shutdown::Both);
        }
    }
}

// ===========================================
// Helper Functions: Handshake Decoding
// ===========================================
fn decode_handshake_packet_ext(buffer: &[u8]) -> io::Result<(String, u32, u32)> {
    let mut cursor = Cursor::new(buffer);
    let _total_len = read_varint(&mut cursor)?; // packet length
    let _packet_id = read_varint(&mut cursor)?;   // handshake packet id
    let protocol_version = read_varint(&mut cursor)?; // protocol version
    let server_address = read_string(&mut cursor)?;   // domain
    let _server_port = cursor.read_u16::<BigEndian>()?;
    let next_state = read_varint(&mut cursor)?;         // 1=status, 2=login
    Ok((server_address, next_state, protocol_version))
}

fn read_string(cursor: &mut Cursor<&[u8]>) -> io::Result<String> {
    let len = read_varint(cursor)? as usize;
    let mut buf = vec![0; len];
    cursor.read_exact(&mut buf)?;
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
            return Err(io::Error::new(io::ErrorKind::InvalidData, "VarInt too long"));
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
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
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
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let key = (domain.to_string(), src_ip);
    let mut entry = DOMAIN_SRC_CONN_COUNT.entry(key).or_insert((now, 0));
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

// ===========================================
// Helper Functions: Blacklist ips
// ===========================================
// Modified: simply check if the IP exists in the map.
fn is_ip_blocked(ip: &IpAddr) -> bool {
    // println!("IP to check {}", ip);
    BLOCKED_IPS.contains_key(ip)
}

pub(crate) fn block_ip(ip: IpAddr) {
    let expiry = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 300;
    BLOCKED_IPS.insert(ip, expiry);
}

// ===========================================
// Helper Functions: Status / Ping Responses
// ===========================================
fn send_status_response(stream: &mut TcpStream, status_json: &str, client_protocol: u32) -> io::Result<()> {
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

    stream.write_all(&packet)?;
    stream.flush()?;
    handle_ping(stream)?;
    Ok(())
}

fn send_fallback_status_response(stream: &mut TcpStream) -> io::Result<()> {
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

    stream.write_all(&packet)?;
    stream.flush()?;
    handle_ping(stream)?;
    Ok(())
}

fn adjust_status_response_for_client(status_json: &str, client_protocol: u32) -> Result<String, serde_json::Error> {
    let mut value: Value = serde_json::from_str(status_json)?;
    if let Some(version) = value.get_mut("version") {
        version["protocol"] = serde_json::json!(client_protocol);
    }
    serde_json::to_string(&value)
}

fn handle_ping(stream: &mut TcpStream) -> io::Result<()> {
    loop {
        let _len = read_varint(stream)?;
        let packet_id = read_varint(stream)?;
        match packet_id {
            0x00 => {
                log_debug!("Received additional status request instead of ping");
            }
            0x01 => {
                let mut payload = [0u8; 8];
                stream.read_exact(&mut payload)?;
                let mut pong = Vec::new();
                pong.push(0x01);
                pong.extend_from_slice(&payload);

                let mut pong_packet = Vec::new();
                write_varint(pong.len() as u32, &mut pong_packet);
                pong_packet.extend_from_slice(&pong);
                stream.write_all(&pong_packet)?;
                stream.flush()?;
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

fn send_initial_buffer_to_target(initial: &[u8], sender: &mut TcpStream) {
    if sender.write_all(initial).is_err() || sender.flush().is_err() {
        log_error!("Failed to send initial buffer to target");
    }
}
