use log::{debug, error, info};
use std::io::{Cursor, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::{io, thread};
use byteorder::{BigEndian, ReadBytesExt};

pub struct TcpProxy {
    pub forward_thread: thread::JoinHandle<()>,
}

/// Reads a VarInt from the cursor, handling variable-length integers used in Minecraft protocol.
fn read_varint(cursor: &mut Cursor<&[u8]>) -> io::Result<i32> {
    let mut num_read = 0;
    let mut result = 0;
    let mut read: u8;

    loop {
        read = cursor.read_u8()?;
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

/// Reads a UTF-8 string from the cursor, using a VarInt length prefix.
fn read_string(cursor: &mut Cursor<&[u8]>) -> io::Result<String> {
    let length = read_varint(cursor)? as usize;
    let mut buffer = vec![0; length];
    cursor.read_exact(&mut buffer)?;
    Ok(String::from_utf8(buffer).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?)
}

/// Decodes the handshake packet to extract the server address.
fn decode_handshake_packet(buffer: &[u8]) -> io::Result<String> {
    let mut cursor = Cursor::new(buffer);
    // Skip reading length and packet ID as we are only interested in the server address
    let _ = read_varint(&mut cursor)?;
    let _ = read_varint(&mut cursor)?;
    let _ = read_varint(&mut cursor)?;
    let server_address = read_string(&mut cursor)?;
    let _ = cursor.read_u16::<BigEndian>()?;
    let _ = read_varint(&mut cursor)?;
    Ok(server_address)
}

/// Handles incoming client connections, processing the initial buffer to extract server address.
fn handle_client(mut stream_forward: TcpStream) {
    let mut initial_buffer = vec![0; 1024];
    let n = match stream_forward.read(&mut initial_buffer) {
        Ok(n) => n,
        Err(e) => {
            error!("Failed to read from the stream: {}", e);
            return;
        }
    };
    debug!("Initial buffer: {:?}", &initial_buffer[..n]);
    match decode_handshake_packet(&initial_buffer) {
        Ok(server_address) => {
            println!("Server Address: {}", server_address);

            let proxy_to: SocketAddr = "91.107.215.249:25577".parse().expect("Invalid address");

            if let Ok(mut sender_forward) = TcpStream::connect(proxy_to) {
                debug!("Connected to target server");
                let mut sender_backward = sender_forward.try_clone().expect("Failed to clone stream");
                let mut stream_backward = stream_forward.try_clone().expect("Failed to clone stream");
                send_initial_buffer_to_target(&initial_buffer, n, &mut sender_forward);
                spawn_client_to_target_thread(stream_forward, sender_forward);
                spawn_target_to_client_thread(sender_backward, stream_backward);
            } else {
                error!("Failed to connect to target");
            }
        },
        Err(e) => eprintln!("Failed to decode packet: {}", e),
    }
}

/// Sends the initial buffer to the target server.
fn send_initial_buffer_to_target(initial_buffer: &[u8], n: usize, sender_forward: &mut TcpStream) {
    if sender_forward.write_all(&initial_buffer[..n]).is_err() {
        error!("Failed to write initial buffer to target");
        return;
    }
    if sender_forward.flush().is_err() {
        error!("Failed to flush initial buffer to target");
    }
}

/// Spawns a thread to handle communication from the client to the target server.
fn spawn_client_to_target_thread(mut stream_forward: TcpStream, mut sender_forward: TcpStream) {
    thread::spawn(move || {
        let mut buffer = vec![0; 1024];
        let mut initial_data_sent = false;
        loop {
            let n = if initial_data_sent {
                match stream_forward.read(&mut buffer) {
                    Ok(n) => n,
                    Err(e) => {
                        error!("Failed to read from client: {}", e);
                        break;
                    }
                }
            } else {
                initial_data_sent = true;
                buffer.len()
            };
            if n == 0 {
                debug!("Client closed connection");
                break;
            }
            if sender_forward.write_all(&buffer[..n]).is_err() {
                error!("Failed to write to target");
                break;
            }
            if sender_forward.flush().is_err() {
                error!("Failed to flush to target");
                break;
            }
        }
    });
}

/// Spawns a thread to handle communication from the target server to the client.
fn spawn_target_to_client_thread(mut sender_backward: TcpStream, mut stream_backward: TcpStream) {
    thread::spawn(move || {
        let mut buffer = vec![0; 1024];
        loop {
            let n = match sender_backward.read(&mut buffer) {
                Ok(n) => n,
                Err(e) => {
                    error!("Failed to read from target: {}", e);
                    break;
                }
            };
            if n == 0 {
                debug!("Target closed connection");
                break;
            }
            if stream_backward.write_all(&buffer[..n]).is_err() {
                error!("Failed to write to client");
                break;
            }
            if stream_backward.flush().is_err() {
                error!("Failed to flush to client");
                break;
            }
        }
    });
}

impl TcpProxy {
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Bind the proxy to listen on port 25565
        let listener_forward = TcpListener::bind(("0.0.0.0", 25565))?;
        info!("Starting proxy on port 25565");

        // Spawn a new thread to handle incoming connections
        let forward_thread = thread::spawn(move || {
            loop {
                match listener_forward.accept() {
                    Ok((stream_forward, _addr)) => {
                        debug!("New connection");
                        handle_client(stream_forward);
                    }
                    Err(e) => error!("Failed to accept connection: {}", e),
                }
            }
        });

        Ok(Self { forward_thread })
    }
}
