// ===========================================
// Imports
// ===========================================
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::thread;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};
use byteorder::{BigEndian, ReadBytesExt};
use proxy_protocol::{
    version2::{ProxyAddresses, ProxyCommand, ProxyTransportProtocol},
    ProxyHeader,
};
use rayon::ThreadPoolBuilder;
use serde_json::Value;
use log::{debug, error, info};
use lazy_static::lazy_static;
use std::sync::Mutex;

use crate::config_loader::{
    resolve, RedirectionConfig, PROXY_THREADS, BIND_ADDRESS,
};
use crate::{forwarding, target_pinger};

// ===========================================
// Variable maps
// ===========================================
lazy_static! {
    // (IpAddr) -> (block_expiry_timestamp)
    pub static ref BLOCKED_IPS: Mutex<HashMap<IpAddr, u64>> = Mutex::new(HashMap::new());
    // (domain, source_ip) -> (timestamp_in_seconds, ping_count_this_second)
    static ref DOMAIN_SRC_PING_COUNT: Mutex<HashMap<(String, IpAddr), (u64, usize)>> = Mutex::new(HashMap::new());
    // (domain, source_ip) -> (timestamp_in_seconds, ping_count_this_second)
    static ref DOMAIN_SRC_CONN_COUNT: Mutex<HashMap<(String, IpAddr), (u64, usize)>> = Mutex::new(HashMap::new());
}


// ===========================================
// Main Proxy Struct & Entry Point
// ===========================================
pub struct TcpProxy {
    pub forward_thread: thread::JoinHandle<()>,
}

impl TcpProxy {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let bind_addr = *BIND_ADDRESS.lock().unwrap();
        let listener = TcpListener::bind(bind_addr)?;

        let num_threads = *PROXY_THREADS.lock().unwrap();
        let pool = ThreadPoolBuilder::new().num_threads(num_threads).build()?;

        let forward_thread = thread::spawn(move || {
            for stream_result in listener.incoming() {
                match stream_result {
                    Ok(stream) => {
                        // Disable Nagle for lower latency
                        if let Err(e) = stream.set_nodelay(true) {
                            error!("Failed to disable Nagle: {}", e);
                        }
                        let src_ip = match stream.peer_addr() {
                            Ok(addr) => addr.ip(),
                            Err(e) => {
                                error!("Failed to get peer address: {}", e);
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
                    Err(e) => error!("Accept error: {}", e),
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
            error!("Failed to get peer address: {}", e);
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
            error!("Read error from {}: {}", src_ip, e);
            let _ = client_stream.shutdown(Shutdown::Both);
            return;
        }
    };
    buf.truncate(n);

    // 3. Decode the handshake packet; block the IP if decoding fails.
    let (server_address, next_state, client_protocol) = match decode_handshake_packet_ext(&buf) {
        Ok(res) => res,
        Err(e) => {
            error!("Handshake decoding error from {}: {}", src_ip, e);
            block_ip(src_ip);
            let _ = client_stream.shutdown(Shutdown::Both);
            return;
        }
    };

    // 4. Process based on the request type: status (ping) or login.
    match next_state {
        1 => {
            // Status request: Check rate limiting if a configuration is available.
            if let Some(cfg) = resolve(&server_address) {
                if !check_ping_limit(&server_address, src_ip, &cfg) {
                    error!("Too many pings for {} from {}", server_address, src_ip);
                    block_ip(src_ip);
                    let _ = client_stream.shutdown(Shutdown::Both);
                    return;
                }
            }
            // Send status response.
            let response = if let Some(status_json) =
                target_pinger::STATUS_CACHE.lock().unwrap().get(&server_address).cloned()
            {
                send_status_response(&mut client_stream, &status_json, client_protocol)
            } else {
                send_fallback_status_response(&mut client_stream)
            };
            if let Err(e) = response {
                error!("Error sending status response: {}", e);
            }
        }
        2 => {
            // Login request: Check domain filtering and connection rate limit.
            if let Some(redirection_cfg) = resolve(&server_address) {
                if check_connection_limit(&server_address, src_ip, &redirection_cfg) {
                    if !check_encryption_if_needed(&redirection_cfg) {
                        error!("Encryption check failed for domain {}", server_address);
                        let _ = client_stream.shutdown(Shutdown::Both);
                        return;
                    }
                    // Connect to the target server.
                    let proxy_to = SocketAddr::new(redirection_cfg.ip.into(), redirection_cfg.port);
                    match TcpStream::connect(proxy_to) {
                        Ok(mut target_stream) => {
                            if let Err(e) = target_stream.set_nodelay(true) {
                                error!("Failed to disable Nagle on target: {}", e);
                            }
                            if let (Ok(src_addr), Ok(dst_addr)) = (client_stream.peer_addr(), target_stream.peer_addr()) {
                                let new_buf = encapsulate_with_proxy_protocol(src_addr, dst_addr, &buf);
                                send_initial_buffer_to_target(&new_buf, &mut target_stream);
                            }
                            // Set up bidirectional forwarding on separate threads.
                            let target_clone = match target_stream.try_clone() {
                                Ok(tc) => tc,
                                Err(e) => {
                                    error!("Error cloning target stream: {}", e);
                                    return;
                                }
                            };
                            let client_clone = match client_stream.try_clone() {
                                Ok(cc) => cc,
                                Err(e) => {
                                    error!("Error cloning client stream: {}", e);
                                    return;
                                }
                            };
                            let domain_clone = server_address.clone();
                            thread::spawn(move || {
                                forwarding::forward_loop(client_clone, target_stream, server_address, src_ip, true, "client->target")
                            });
                            thread::spawn(move || {
                                forwarding::forward_loop(target_clone, client_stream, domain_clone, src_ip, false, "target->client")
                            });
                        }
                        Err(e) => {
                            error!("Error connecting to target server for {}: {}", server_address, e);
                            let _ = client_stream.shutdown(Shutdown::Both);
                        }
                    }
                } else {
                    error!("Connection rate limit exceeded for {} from {}", server_address, src_ip);
                    block_ip(src_ip);
                    let _ = client_stream.shutdown(Shutdown::Both);
                    return;
                }
            } else {
                info!("No redirection configuration for domain {}, sending fallback response", server_address);
                if let Err(e) = send_fallback_status_response(&mut client_stream) {
                    error!("Error sending fallback response: {}", e);
                }
            }
        }
        other => {
            error!("Unknown next_state ({}) from {} for domain {}", other, src_ip, server_address);
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
    let mut map = DOMAIN_SRC_PING_COUNT.lock().unwrap();
    let entry = map.entry((domain.to_string(), src_ip)).or_insert((now, 0));

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
    // Make sure RedirectionConfig includes a field like max_connections_per_second
    let limit = cfg.max_connections_per_second;
    if limit == 0 {
        return true; // unlimited
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut map = DOMAIN_SRC_CONN_COUNT.lock().unwrap();
    let entry = map.entry((domain.to_string(), src_ip)).or_insert((now, 0));

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
/// Filter incoming connections based on ip
fn is_ip_blocked(ip: &IpAddr) -> bool {
    let mut blocked = BLOCKED_IPS.lock().unwrap();
    if let Some(&expiry) = blocked.get(ip) {
        if SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() < expiry {
            return true;
        } else {
            // Remove if expired
            blocked.remove(ip);
        }
    }
    false
}
pub(crate) fn block_ip(ip: IpAddr) {
    let expiry = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 300;
    BLOCKED_IPS.lock().unwrap().insert(ip, expiry);
}

// ===========================================
// Helper Functions: Status / Ping Responses
// ===========================================
fn send_status_response(stream: &mut TcpStream, status_json: &str, client_protocol: u32) -> io::Result<()> {
    let adjusted_json = adjust_status_response_for_client(status_json, client_protocol)
        .unwrap_or_else(|e| {
            error!("Failed to adjust status response: {}", e);
            status_json.to_owned()
        });

    // Build the response packet
    let mut resp = Vec::new();
    resp.push(0x00);
    write_varint(adjusted_json.len() as u32, &mut resp);
    resp.extend_from_slice(adjusted_json.as_bytes());

    let mut packet = Vec::new();
    write_varint(resp.len() as u32, &mut packet);
    packet.extend_from_slice(&resp);

    // Send response and then handle ping
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
                debug!("Received additional status request instead of ping");
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
        error!("Failed to send initial buffer to target");
    }
}

/// Stub for encryption check.
fn check_encryption_if_needed(_redirection_cfg: &RedirectionConfig) -> bool {
    true
}
