use std::collections::HashMap;
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::thread;
use byteorder::{BigEndian, ReadBytesExt};
use proxy_protocol::{
    version2::{ProxyAddresses, ProxyCommand, ProxyTransportProtocol},
    ProxyHeader,
};
use rayon::ThreadPoolBuilder;
use serde_json::Value;
use log::{debug, error, info};

// Import update_service functions and types.
use crate::update_service::{resolve, try_register_connection, ConnectionGuard, PROXY_THREADS, BIND_ADDRESS};
// Import the pinger module for status caching.
use crate::pinger;

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
                    Ok(mut stream) => {
                        if let Err(e) = stream.set_nodelay(true) {
                            error!("Failed to disable Nagle: {}", e);
                        }
                        let src_ip = match stream.peer_addr() {
                            Ok(addr) => addr.ip(),
                            Err(e) => {
                                error!("Failed to get peer address: {}", e);
                                let _ = stream.shutdown(Shutdown::Both);
                                return;
                            }
                        };
                        if is_ip_blocked(&src_ip) {
                            let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                            let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(1)));
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

fn is_ip_blocked(_ip: &IpAddr) -> bool {
    false
}

fn to_domain_filter(_ip: &IpAddr, _domain: &str) -> bool {
    false
}

fn adjust_status_response_for_client(status_json: &str, client_protocol: u32) -> Result<String, serde_json::Error> {
    let mut value: Value = serde_json::from_str(status_json)?;
    if let Some(version) = value.get_mut("version") {
        version["protocol"] = serde_json::json!(client_protocol);
    }
    serde_json::to_string(&value)
}

fn send_status_response(stream: &mut TcpStream, status_json: &str, client_protocol: u32) -> io::Result<()> {
    let adjusted_json = adjust_status_response_for_client(status_json, client_protocol)
        .unwrap_or_else(|e| {
            error!("Failed to adjust status response: {}", e);
            status_json.to_owned()
        });
    let mut resp = Vec::with_capacity(64);
    resp.push(0x00);
    write_varint(adjusted_json.len() as u32, &mut resp);
    resp.extend_from_slice(adjusted_json.as_bytes());

    let mut packet = Vec::with_capacity(64);
    write_varint(resp.len() as u32, &mut packet);
    packet.extend_from_slice(&resp);
    stream.write_all(&packet)?;
    stream.flush()?;

    handle_ping(stream)?;
    Ok(())
}

fn send_fallback_status_response(stream: &mut TcpStream) -> io::Result<()> {
    let protocol_version = 754;
    let json = format!(r#"{{
  "version": {{"name": "1.21.1", "protocol": {}}},
  "players": {{"max": 0, "online": 0, "sample": []}},
  "description": {{"text": "The given destination does not exist!"}}
}}"#, protocol_version);

    let mut resp = Vec::with_capacity(64);
    resp.push(0x00);
    write_varint(json.len() as u32, &mut resp);
    resp.extend_from_slice(json.as_bytes());

    let mut packet = Vec::with_capacity(64);
    write_varint(resp.len() as u32, &mut packet);
    packet.extend_from_slice(&resp);
    stream.write_all(&packet)?;
    stream.flush()?;

    handle_ping(stream)?;
    Ok(())
}

fn write_varint(mut value: u32, buf: &mut Vec<u8>) {
    while value > 0x7F {
        buf.push(((value & 0x7F) as u8) | 0x80);
        value >>= 7;
    }
    buf.push(value as u8);
}

/// Reads a VarInt from any Read implementation.
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
        if byte & 0x80 == 0 { break; }
    }
    Ok(result)
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
                let mut pong = Vec::with_capacity(16);
                pong.push(0x01);
                pong.extend_from_slice(&payload);
                let mut pong_packet = Vec::with_capacity(16);
                write_varint(pong.len() as u32, &mut pong_packet);
                pong_packet.extend_from_slice(&pong);
                stream.write_all(&pong_packet)?;
                stream.flush()?;
                return Ok(());
            }
            _ => return Err(io::Error::new(io::ErrorKind::InvalidData, format!("Unexpected packet id in handle_ping: 0x{:02X}", packet_id))),
        }
    }
}

fn handle_client(mut stream_forward: TcpStream) {
    let src_ip = match stream_forward.peer_addr() {
        Ok(addr) => addr.ip(),
        Err(e) => {
            error!("Failed to get peer address: {}", e);
            let _ = stream_forward.shutdown(Shutdown::Both);
            return;
        }
    };

    let mut buf = vec![0; 1024];
    let n = match stream_forward.read(&mut buf) {
        Ok(n) => n,
        Err(e) => {
            error!("Read error: {}", e);
            return;
        }
    };

    match decode_handshake_packet_ext(&buf[..n]) {
        Ok((server_address, next_state, client_protocol)) => {
            if next_state == 1 {
                info!("Status request for {} received", server_address);
                let cached = pinger::STATUS_CACHE.lock().unwrap().get(&server_address).cloned();
                let res = if let Some(status_json) = cached {
                    send_status_response(&mut stream_forward, &status_json, client_protocol)
                } else {
                    send_fallback_status_response(&mut stream_forward)
                };
                if let Err(e) = res {
                    error!("Failed to send status response: {}", e);
                }
            } else if next_state == 2 {
                if let Some((ip, port)) = resolve(server_address.clone()) {
                    if let Some(guard) = try_register_connection(&server_address) {
                        let proxy_to = SocketAddr::new(ip.into(), port);
                        if to_domain_filter(&src_ip, &server_address) {
                            info!("Connection from {} to {} denied by filter", src_ip, server_address);
                            let _ = stream_forward.shutdown(Shutdown::Both);
                            return;
                        }
                        if let Ok(mut sender_forward) = TcpStream::connect(proxy_to) {
                            if let Err(e) = sender_forward.set_nodelay(true) {
                                error!("Failed to disable Nagle on target: {}", e);
                            }
                            let sender_backward = sender_forward.try_clone().expect("Clone failed");
                            let stream_backward = stream_forward.try_clone().expect("Clone failed");

                            if let (Ok(src_addr), Ok(dst_addr)) = (stream_forward.peer_addr(), sender_forward.peer_addr()) {
                                let new_buf = encapsulate_with_proxy_protocol(src_addr, dst_addr, &buf[..n]);
                                send_initial_buffer_to_target(&new_buf, &mut sender_forward);
                            }
                            let guard_client = guard.clone();
                            let guard_target = guard.clone();
                            spawn_client_to_target_thread(stream_forward, sender_forward, guard_client);
                            spawn_target_to_client_thread(sender_backward, stream_backward, guard_target);
                        } else {
                            error!("Failed to connect to target");
                        }
                    } else {
                        info!("Rate limit exceeded for target {}", server_address);
                        let _ = stream_forward.shutdown(Shutdown::Both);
                    }
                } else {
                    info!("Target {} not found, sending fallback", server_address);
                    let _ = send_fallback_status_response(&mut stream_forward);
                }
            }
        }
        Err(e) => error!("Handshake decode error: {}", e),
    }
}

fn encode_proxy_protocol_v2_header(src: SocketAddr, dst: SocketAddr) -> Vec<u8> {
    let proxy_addr = match (src, dst) {
        (SocketAddr::V4(s), SocketAddr::V4(d)) => ProxyAddresses::Ipv4 { source: s, destination: d },
        _ => unreachable!(),
    };
    proxy_protocol::encode(ProxyHeader::Version2 {
        command: ProxyCommand::Proxy,
        transport_protocol: ProxyTransportProtocol::Stream,
        addresses: proxy_addr,
    }).unwrap().to_vec()
}

fn encapsulate_with_proxy_protocol(src: SocketAddr, dst: SocketAddr, data: &[u8]) -> Vec<u8> {
    let header = encode_proxy_protocol_v2_header(src, dst);
    let mut buf = Vec::with_capacity(header.len() + data.len());
    buf.extend_from_slice(&header);
    buf.extend_from_slice(data);
    buf
}

fn send_initial_buffer_to_target(initial: &[u8], sender: &mut TcpStream) {
    if sender.write_all(initial).is_err() || sender.flush().is_err() {
        error!("Failed to send initial buffer to target");
    }
}

fn spawn_client_to_target_thread(mut client: TcpStream, mut target: TcpStream, _guard: std::sync::Arc<ConnectionGuard>) {
    thread::spawn(move || {
        let mut buf = vec![0; 1024];
        loop {
            match client.read(&mut buf) {
                Ok(n) if n > 0 => {
                    if target.write_all(&buf[..n]).is_err() || target.flush().is_err() {
                        error!("Failed to forward data to target");
                        break;
                    }
                }
                Ok(_) => break,
                Err(e) => {
                    error!("Client read error: {}", e);
                    break;
                }
            }
        }
        let _ = target.shutdown(Shutdown::Write);
    });
}

fn spawn_target_to_client_thread(mut target: TcpStream, mut client: TcpStream, _guard: std::sync::Arc<ConnectionGuard>) {
    thread::spawn(move || {
        let mut buf = vec![0; 2048];
        loop {
            match target.read(&mut buf) {
                Ok(n) if n > 0 => {
                    if client.write_all(&buf[..n]).is_err() || client.flush().is_err() {
                        error!("Failed to forward data to client");
                        break;
                    }
                }
                Ok(_) => break,
                Err(e) => {
                    error!("Target read error: {}", e);
                    break;
                }
            }
        }
        let _ = client.shutdown(Shutdown::Write);
    });
}

fn read_string(cursor: &mut Cursor<&[u8]>) -> io::Result<String> {
    let len = read_varint(cursor)? as usize;
    let mut buf = vec![0; len];
    cursor.read_exact(&mut buf)?;
    String::from_utf8(buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn decode_handshake_packet(buffer: &[u8]) -> io::Result<String> {
    let mut cursor = Cursor::new(buffer);
    let _ = read_varint(&mut cursor)?;
    let _ = read_varint(&mut cursor)?;
    let _ = read_varint(&mut cursor)?;
    let server_address = read_string(&mut cursor)?;
    let _ = cursor.read_u16::<BigEndian>()?;
    let _ = read_varint(&mut cursor)?;
    Ok(server_address)
}

fn decode_handshake_packet_ext(buffer: &[u8]) -> io::Result<(String, u32, u32)> {
    let mut cursor = Cursor::new(buffer);
    let _ = read_varint(&mut cursor)?;
    let _ = read_varint(&mut cursor)?;
    let protocol_version = read_varint(&mut cursor)? as u32;
    let server_address = read_string(&mut cursor)?;
    let _ = cursor.read_u16::<BigEndian>()?;
    let next_state = read_varint(&mut cursor)? as u32;
    Ok((server_address, next_state, protocol_version))
}
