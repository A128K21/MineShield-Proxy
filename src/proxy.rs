use log::{debug, error, info};
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::{io, thread};
use std::collections::{HashMap, HashSet};
use byteorder::{BigEndian, ReadBytesExt};
use proxy_protocol::{
    version2::{ProxyAddresses, ProxyCommand, ProxyTransportProtocol},
    ProxyHeader,
};
use bytes::BufMut;
use rayon::ThreadPoolBuilder;

// Import functions and types from update_service.
use crate::update_service::{resolve, try_register_connection, ConnectionGuard, PROXY_THREADS};

pub struct TcpProxy {
    pub forward_thread: thread::JoinHandle<()>,
}

impl TcpProxy {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let listener_forward = TcpListener::bind(("0.0.0.0", 25565))?;
        info!("Starting proxy on port 25565");

        // Use the proxy_threads value from the configuration.
        let num_threads = *PROXY_THREADS.lock().unwrap();
        let pool = ThreadPoolBuilder::new().num_threads(num_threads).build().unwrap();

        let forward_thread = thread::spawn(move || {
            loop {
                match listener_forward.accept() {
                    Ok((stream_forward, _addr)) => {
                        let src_ip = _addr.ip();
                        if is_ip_blocked(&src_ip) {
                            let _ = stream_forward.set_read_timeout(Some(std::time::Duration::from_secs(1)));
                            let _ = stream_forward.set_write_timeout(Some(std::time::Duration::from_secs(1)));
                            let _ = stream_forward.shutdown(Shutdown::Both);
                            continue;
                        }
                        pool.spawn(|| handle_client(stream_forward));
                    }
                    Err(e) => error!("Failed to accept connection: {}", e),
                }
            }
        });

        Ok(Self { forward_thread })
    }
}

/// Dummy IP filtering function. Replace with your own rules.
fn is_ip_blocked(src_ip: &IpAddr) -> bool {
    false
}

/// Dummy domain filter. Replace with your own logic.
fn to_domain_filter(_src_ip: &IpAddr, _domain: &str) -> bool {
    false
}

/// Handles an incoming client connection.
fn handle_client(mut stream_forward: TcpStream) {
    let src_ip = match stream_forward.peer_addr() {
        Ok(addr) => addr.ip(),
        Err(e) => {
            error!("Failed to get peer address: {}", e);
            let _ = stream_forward.shutdown(Shutdown::Both);
            return;
        }
    };

    let mut initial_buffer = vec![0; 1024];
    let n = match stream_forward.read(&mut initial_buffer) {
        Ok(n) => n,
        Err(e) => {
            error!("Failed to read from stream: {}", e);
            return;
        }
    };

    match decode_handshake_packet(&initial_buffer) {
        Ok(server_address) => {
            let server_address_clone = server_address.clone();

            if let Some((ip, port)) = resolve(server_address) {
                if let Some(guard) = try_register_connection(&server_address_clone) {
                    let proxy_to: SocketAddr = SocketAddr::new(ip.into(), port);

                    if to_domain_filter(&src_ip, &server_address_clone) {
                        info!("Connection from {} to {} denied by filter", src_ip, server_address_clone);
                        let _ = stream_forward.shutdown(Shutdown::Both);
                        return;
                    }

                    if let Ok(mut sender_forward) = TcpStream::connect(proxy_to) {
                        let sender_backward = sender_forward.try_clone().expect("Failed to clone stream");
                        let stream_backward = stream_forward.try_clone().expect("Failed to clone stream");

                        if let (Ok(src_addr), Ok(dst_addr)) = (stream_forward.peer_addr(), sender_forward.peer_addr()) {
                            let new_buffer = encapsulate_with_proxy_protocol(src_addr, dst_addr, &initial_buffer[..n]);
                            send_initial_buffer_to_target(&new_buffer, &mut sender_forward);
                        }

                        let guard_client = guard.clone();
                        let guard_target = guard.clone();
                        spawn_client_to_target_thread(stream_forward, sender_forward, guard_client);
                        spawn_target_to_client_thread(sender_backward, stream_backward, guard_target);
                    } else {
                        error!("Failed to connect to target");
                    }
                } else {
                    info!("Rate limit exceeded for target {}", server_address_clone);
                    let _ = stream_forward.shutdown(Shutdown::Both);
                    return;
                }
            }
        }
        Err(e) => error!("Failed to decode packet: {}", e),
    }
}

/// Encodes the Proxy Protocol v2 header.
fn encode_proxy_protocol_v2_header(src_addr: SocketAddr, dst_addr: SocketAddr) -> Vec<u8> {
    let proxy_addr = match (src_addr, dst_addr) {
        (SocketAddr::V4(source), SocketAddr::V4(destination)) => ProxyAddresses::Ipv4 { source, destination },
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

/// Encapsulates the initial data with the Proxy Protocol header.
fn encapsulate_with_proxy_protocol(src_addr: SocketAddr, dst_addr: SocketAddr, data: &[u8]) -> Vec<u8> {
    let header = encode_proxy_protocol_v2_header(src_addr, dst_addr);
    let mut new_buffer = Vec::with_capacity(header.len() + data.len());
    new_buffer.extend_from_slice(&header);
    new_buffer.extend_from_slice(data);
    new_buffer
}

/// Sends the initial buffer to the target.
fn send_initial_buffer_to_target(initial_buffer: &[u8], sender_forward: &mut TcpStream) {
    if sender_forward.write_all(initial_buffer).is_err() || sender_forward.flush().is_err() {
        error!("Failed to send initial buffer to target");
    }
}

/// Spawns a thread to forward data from the client to the target.
/// The passed `_guard` is held until the thread ends.
fn spawn_client_to_target_thread(mut stream_forward: TcpStream, mut sender_forward: TcpStream, _guard: std::sync::Arc<ConnectionGuard>) {
    thread::spawn(move || {
        let mut buffer = vec![0; 1024];
        loop {
            match stream_forward.read(&mut buffer) {
                Ok(n) if n > 0 => {
                    if sender_forward.write_all(&buffer[..n]).is_err() || sender_forward.flush().is_err() {
                        error!("Failed to forward data to target");
                        break;
                    }
                }
                Ok(_) => break,
                Err(e) => {
                    error!("Failed to read from client: {}", e);
                    break;
                }
            }
        }
        let _ = sender_forward.shutdown(Shutdown::Write);
    });
}

/// Spawns a thread to forward data from the target back to the client.
/// The passed `_guard` is held until the thread ends.
fn spawn_target_to_client_thread(mut sender_backward: TcpStream, mut stream_backward: TcpStream, _guard: std::sync::Arc<ConnectionGuard>) {
    thread::spawn(move || {
        let mut buffer = vec![0; 2048];
        loop {
            match sender_backward.read(&mut buffer) {
                Ok(n) if n > 0 => {
                    if stream_backward.write_all(&buffer[..n]).is_err() || stream_backward.flush().is_err() {
                        error!("Failed to forward data to client");
                        break;
                    }
                }
                Ok(_) => break,
                Err(e) => {
                    error!("Failed to read from target: {}", e);
                    break;
                }
            }
        }
        let _ = stream_backward.shutdown(Shutdown::Write);
    });
}

/// Reads a VarInt from the cursor.
fn read_varint(cursor: &mut Cursor<&[u8]>) -> io::Result<i32> {
    let mut num_read = 0;
    let mut result = 0;
    loop {
        let read = cursor.read_u8()?;
        let value = (read & 0b01111111) as i32;
        result |= value << (7 * num_read);
        num_read += 1;
        if num_read > 5 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "VarInt is too big"));
        }
        if (read & 0b10000000) == 0 {
            break;
        }
    }
    Ok(result)
}

/// Reads a UTF-8 string from the cursor.
fn read_string(cursor: &mut Cursor<&[u8]>) -> io::Result<String> {
    let length = read_varint(cursor)? as usize;
    let mut buffer = vec![0; length];
    cursor.read_exact(&mut buffer)?;
    String::from_utf8(buffer).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Decodes the handshake packet to extract the server address.
fn decode_handshake_packet(buffer: &[u8]) -> io::Result<String> {
    let mut cursor = Cursor::new(buffer);
    let _ = read_varint(&mut cursor)?; // Packet length
    let _ = read_varint(&mut cursor)?; // Packet ID
    let _ = read_varint(&mut cursor)?; // Protocol version
    let server_address = read_string(&mut cursor)?;
    let _ = cursor.read_u16::<BigEndian>()?; // Server port
    let _ = read_varint(&mut cursor)?; // Next state
    Ok(server_address)
}
