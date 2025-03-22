// pinger.rs

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpStream, SocketAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use lazy_static::lazy_static;
use log::{info, error};
use proxy_protocol::{
    version2::{ProxyAddresses, ProxyCommand, ProxyTransportProtocol},
    ProxyHeader,
};
use serde_json::Value;

lazy_static! {
    // Global status cache: key is the domain, value is the latest JSON status.
    pub static ref STATUS_CACHE: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
}

/// Reads a VarInt from the given reader.
pub fn read_varint<R: Read>(reader: &mut R) -> io::Result<u32> {
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
        if (byte & 0x80) == 0 {
            break;
        }
    }
    Ok(result)
}

/// Writes a VarInt into the provided buffer.
pub fn write_varint(mut value: u32, buf: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 { break; }
    }
}

/// Writes a string (length-prefixed as a VarInt) into the buffer.
pub fn write_string(s: &str, buf: &mut Vec<u8>) {
    write_varint(s.len() as u32, buf);
    buf.extend_from_slice(s.as_bytes());
}

/// Builds the proxy-protocol header using the exact same logic as in your proxy code.
pub fn build_proxy_protocol_header(src_addr: SocketAddr, dst_addr: SocketAddr) -> Vec<u8> {
    let proxy_addr = match (src_addr, dst_addr) {
        (SocketAddr::V4(source), SocketAddr::V4(destination)) => {
            ProxyAddresses::Ipv4 { source, destination }
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


/// Builds a handshake packet for a status request.
/// Packet structure:
///   [Packet Length VarInt]
///   [Packet ID VarInt (0x00)]
///   [Protocol Version VarInt]
///   [Server Address (string)]
///   [Port (u16 big endian)]
///   [Next State VarInt (1)]
pub fn build_handshake_packet(server_address: &str, protocol_version: u32, port: u16) -> Vec<u8> {
    let mut packet_data = Vec::new();
    // Handshake packet ID.
    write_varint(0x00, &mut packet_data);
    // Protocol version.
    write_varint(protocol_version, &mut packet_data);
    // Server address.
    write_string(server_address, &mut packet_data);
    // Port (big-endian).
    packet_data.push((port >> 8) as u8);
    packet_data.push((port & 0xFF) as u8);
    // Next state: 1 (status).
    write_varint(1, &mut packet_data);

    let mut packet = Vec::new();
    write_varint(packet_data.len() as u32, &mut packet);
    packet.extend_from_slice(&packet_data);
    packet
}

/// Builds a status request packet (packet id 0x00, no payload).
pub fn build_status_request_packet() -> Vec<u8> {
    let mut data = Vec::new();
    write_varint(0x00, &mut data);
    let mut packet = Vec::new();
    write_varint(data.len() as u32, &mut packet);
    packet.extend_from_slice(&data);
    packet
}

/// Reads the complete status response from the target.
/// It first reads the VarInt length and then reads exactly that many bytes,
/// then decodes the packet (expecting packet id 0x00 and a JSON string).
pub fn read_status_response(stream: &mut TcpStream) -> io::Result<String> {
    // Read full packet length.
    let packet_length = read_varint(stream)? as usize;
    let mut packet_buf = vec![0u8; packet_length];
    stream.read_exact(&mut packet_buf)?;

    let mut cursor = std::io::Cursor::new(packet_buf);
    let packet_id = read_varint(&mut cursor)?;
    if packet_id != 0x00 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Invalid packet id in status response"));
    }
    let json_length = read_varint(&mut cursor)? as usize;
    let mut json_buf = vec![0u8; json_length];
    cursor.read_exact(&mut json_buf)?;
    let json = String::from_utf8(json_buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(json)
}

/// Pings the target server and returns its status JSON.
/// Note: This function sends the proxy-protocol header using the same logic as in your proxy.
/// Pings the target server and returns its status JSON without modifying it.
/// The proxy measures latency and logs it, but the returned JSON remains unchanged.
pub fn ping_target(
    server_address: &str,
    ip: Ipv4Addr,
    port: u16,
    protocol_version: u32,
) -> io::Result<String> {
    let target_addr = SocketAddr::new(ip.into(), port);
    // Connect with a timeout.
    let mut stream = TcpStream::connect_timeout(&target_addr, Duration::from_secs(1))?;
    stream.set_nodelay(true)?;
    // Optionally, set a read timeout (e.g., 2 seconds) to avoid hanging.
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    // Send the proxy-protocol header.
    let local_addr = stream.local_addr()?;
    let proxy_header = build_proxy_protocol_header(local_addr, target_addr);
    stream.write_all(&proxy_header)?;
    stream.flush()?;

    // Record start time.
    let start = Instant::now();

    // Send handshake packet.
    let handshake_packet = build_handshake_packet(server_address, protocol_version, port);
    stream.write_all(&handshake_packet)?;
    stream.flush()?;

    // Send status request packet.
    let status_request = build_status_request_packet();
    stream.write_all(&status_request)?;
    stream.flush()?;

    // Increase delay slightly to ensure the full response is sent.
    std::thread::sleep(Duration::from_millis(150));

    // Read the status response.
    let response = read_status_response(&mut stream)?;
    let latency = start.elapsed().as_millis();

    // Log the measured latency.
    info!("Latency for {}: {} ms", server_address, latency);

    // Return the original JSON response without any injection.
    Ok(response)
}

/// Background thread that pings each target every second and updates the cache.
pub fn background_pinger() {
    loop {
        // Get a copy of the proxy map from update_service.
        let proxy_map = crate::update_service::PROXY_MAP.lock().unwrap().clone();
        for (domain, &(ip, port)) in proxy_map.iter() {
            // Use a fixed protocol version for the ping (e.g. 754).
            let protocol_version = 754;
            match ping_target(domain, ip, port, protocol_version) {
                Ok(status_json) => {
                    info!("Ping successful for {}: {}", domain, status_json);
                    STATUS_CACHE.lock().unwrap().insert(domain.clone(), status_json);
                },
                Err(e) => {
                    error!("Ping failed for {}: {}", domain, e);
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
