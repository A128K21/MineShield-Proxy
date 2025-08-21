// ===========================================
// Imports
// ===========================================
use lazy_static::lazy_static;
use log::{error, info};
use proxy_protocol::{
    version2::{ProxyAddresses, ProxyCommand, ProxyTransportProtocol},
    ProxyHeader,
};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ===========================================
// Global State: Status Cache
// ===========================================
// A global status cache: domain -> latest JSON status.
lazy_static! {
    pub static ref STATUS_CACHE: Mutex<HashMap<String, String>> = Mutex::new(HashMap::new());
}

// ===========================================
// Section 1: VarInt Helper Functions
// ===========================================

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
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VarInt too long",
            ));
        }
        if (byte & 0x80) == 0 {
            break;
        }
    }
    Ok(result)
}

/// Writes a VarInt to the provided buffer.
pub fn write_varint(mut value: u32, buf: &mut Vec<u8>) {
    loop {
        let mut byte = (value & 0x7F) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Appends a VarInt length and a UTF-8 string to the buffer.
pub fn write_string(s: &str, buf: &mut Vec<u8>) {
    write_varint(s.len() as u32, buf);
    buf.extend_from_slice(s.as_bytes());
}

// ===========================================
// Section 2: Proxy Protocol Header Builder
// ===========================================

/// Builds a Proxy Protocol v2 header given the source and destination addresses.
pub fn build_proxy_protocol_header(src_addr: SocketAddr, dst_addr: SocketAddr) -> Vec<u8> {
    let proxy_addr = match (src_addr, dst_addr) {
        (SocketAddr::V4(source), SocketAddr::V4(destination)) => ProxyAddresses::Ipv4 {
            source,
            destination,
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
// Section 3: Minecraft Handshake and Packet Builders
// ===========================================

/// Builds a handshake packet for status requests.
/// Format:
/// [VarInt: packet_len] [VarInt: packet_id=0x00]
/// [VarInt: protocol_version] [String: server_address]
/// [u16: port] [VarInt: next_state=1]
pub fn build_handshake_packet(server_address: &str, protocol_version: u32, port: u16) -> Vec<u8> {
    let mut packet_data = Vec::new();
    // packet_id = 0x00 (handshake)
    write_varint(0x00, &mut packet_data);
    // protocol version
    write_varint(protocol_version, &mut packet_data);
    // server_address
    write_string(server_address, &mut packet_data);
    // port (big-endian)
    packet_data.push((port >> 8) as u8);
    packet_data.push((port & 0xFF) as u8);
    // next_state = 1 (status)
    write_varint(1, &mut packet_data);

    let mut packet = Vec::new();
    write_varint(packet_data.len() as u32, &mut packet);
    packet.extend_from_slice(&packet_data);
    packet
}

/// Builds a status request packet (packet id 0x00, with an empty payload).
pub fn build_status_request_packet() -> Vec<u8> {
    let mut data = Vec::new();
    write_varint(0x00, &mut data);
    let mut packet = Vec::new();
    write_varint(data.len() as u32, &mut packet);
    packet.extend_from_slice(&data);
    packet
}

// ===========================================
// Section 4: Status Response Reading
// ===========================================

/// Reads the status response from the stream.
/// Expected format: [VarInt: length], [VarInt: packet_id=0x00],
/// [VarInt: json_len], [bytes: json]
pub fn read_status_response(stream: &mut TcpStream) -> io::Result<String> {
    let packet_length = read_varint(stream)? as usize;
    let mut packet_buf = vec![0u8; packet_length];
    stream.read_exact(&mut packet_buf)?;

    let mut cursor = std::io::Cursor::new(packet_buf);
    let packet_id = read_varint(&mut cursor)?;
    if packet_id != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid packet id in status response",
        ));
    }
    let json_length = read_varint(&mut cursor)? as usize;
    let mut json_buf = vec![0u8; json_length];
    cursor.read_exact(&mut json_buf)?;
    let json =
        String::from_utf8(json_buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(json)
}

// ===========================================
// Section 5: Ping Target Functionality
// ===========================================

/// Pings the target server using a Proxy Protocol v2 header and returns the status JSON.
pub fn ping_target(
    server_address: &str,
    ip: Ipv4Addr,
    port: u16,
    protocol_version: u32,
) -> io::Result<String> {
    let target_addr = SocketAddr::new(ip.into(), port);
    // Connect with a 1-second timeout
    let mut stream = TcpStream::connect_timeout(&target_addr, Duration::from_secs(1))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;

    // Send Proxy Protocol header
    let local_addr = stream.local_addr()?;
    let proxy_header = build_proxy_protocol_header(local_addr, target_addr);
    stream.write_all(&proxy_header)?;
    stream.flush()?;

    // Start measuring latency
    let start = Instant::now();

    // Send handshake packet
    let handshake_packet = build_handshake_packet(server_address, protocol_version, port);
    stream.write_all(&handshake_packet)?;
    stream.flush()?;

    // Send status request packet
    let status_request = build_status_request_packet();
    stream.write_all(&status_request)?;
    stream.flush()?;

    // Wait briefly to ensure the full response is received
    std::thread::sleep(Duration::from_millis(150));

    // Read status response
    let response = read_status_response(&mut stream)?;
    let latency = start.elapsed().as_millis();
    info!("Latency for '{}': {} ms", server_address, latency);

    // Return the JSON status
    Ok(response)
}

// ===========================================
// Section 6: Background Pinger Thread
// ===========================================

/// Background thread that continuously pings each redirection domain and updates the STATUS_CACHE.
pub fn background_pinger() {
    loop {
        // Clone the redirection map from update_service (domain -> RedirectionConfig)
        let redirection_map: Vec<(String, crate::config_loader::RedirectionConfig)> =
            crate::config_loader::REDIRECTION_MAP
                .iter()
                .map(|e| (e.key().clone(), e.value().clone()))
                .collect();

        for (domain, redirection_cfg) in redirection_map {
            // Retrieve target IP and port from the configuration.
            let ip = redirection_cfg.ip;
            let port = redirection_cfg.port;

            // Use protocol version 754 (e.g., for Minecraft 1.16.4+)
            let protocol_version = 754;

            match ping_target(&domain, ip, port, protocol_version) {
                Ok(status_json) => {
                    info!("Ping successful for '{}'", domain);
                    // Update the JSON status in the cache.
                    STATUS_CACHE
                        .lock()
                        .unwrap()
                        .insert(domain.clone(), status_json);
                }
                Err(e) => {
                    error!("Ping failed for '{}': {}", domain, e);
                }
            }
        }

        // Sleep for 1 second before the next round of pings.
        std::thread::sleep(Duration::from_secs(1));
    }
}
