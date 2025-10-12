use log::{debug, trace};
use minecraft_protocol::prelude::{BinaryReader, BinaryReaderError, VarInt};
use net::packet_stream::{PacketStream, PacketStreamError};
use net::raw_packet::RawPacket;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::time::{sleep, Duration, Instant};

#[derive(Debug)]
pub struct HandshakeCapture {
    buffer: Vec<u8>,
    header_len: usize,
    packet_len: usize,
}

impl HandshakeCapture {
    pub fn buffer(&self) -> &[u8] {
        &self.buffer
    }

    pub fn into_parts(self) -> (Vec<u8>, usize, usize) {
        (self.buffer, self.header_len, self.packet_len)
    }

    pub fn into_buffer(self) -> Vec<u8> {
        self.buffer
    }
}

pub async fn capture_handshake(
    stream: &mut TcpStream,
    mut initial: Vec<u8>,
    read_timeout: Duration,
) -> io::Result<HandshakeCapture> {
    let mut header: Option<(usize, usize)> = try_parse_packet_header(&initial)?;

    while header.is_none() {
        let mut byte = [0u8; 1];
        let read = with_timeout(stream.read(&mut byte), read_timeout).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Client disconnected before handshake finished",
            ));
        }
        initial.extend_from_slice(&byte[..read]);
        header = try_parse_packet_header(&initial)?;
    }

    let (packet_len, header_len) = header.unwrap();
    let total_len = packet_len + header_len;
    debug!(
        "Captured handshake header_len={} packet_len={} (current {})",
        header_len,
        packet_len,
        initial.len()
    );

    while initial.len() < total_len {
        let mut buf = vec![0u8; total_len - initial.len()];
        let read = with_timeout(stream.read(&mut buf), read_timeout).await?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Client disconnected before handshake body finished",
            ));
        }
        initial.extend_from_slice(&buf[..read]);
    }

    Ok(HandshakeCapture {
        buffer: initial,
        header_len,
        packet_len,
    })
}

pub async fn verify_client_stream(
    stream: &mut TcpStream,
    capture: HandshakeCapture,
    client_protocol: u32,
    handshake_timeout: Duration,
    hold_duration: Duration,
) -> io::Result<Vec<u8>> {
    debug!(
        "Starting PicoLimbo stream verification (protocol {}, hold {} ms)",
        client_protocol,
        hold_duration.as_millis()
    );

    let (mut buffer, header_len, packet_len) = capture.into_parts();
    let handshake_end = header_len + packet_len;
    let mut trailing = if buffer.len() > handshake_end {
        buffer.split_off(handshake_end)
    } else {
        Vec::new()
    };
    buffer.truncate(handshake_end);

    let wrapped = BufferedStream::new(stream, std::mem::take(&mut trailing));
    let mut packet_stream = PacketStream::new(wrapped);

    wait_for_login_start(&mut packet_stream, handshake_timeout, &mut buffer).await?;

    hold_connection(&mut packet_stream, hold_duration, &mut buffer).await?;

    debug!(
        "PicoLimbo verification complete ({} bytes captured)",
        buffer.len()
    );
    Ok(buffer)
}

async fn wait_for_login_start(
    packet_stream: &mut PacketStream<BufferedStream<'_>>,
    timeout: Duration,
    capture: &mut Vec<u8>,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "Timed out waiting for login start during limbo verification",
            ));
        }
        let packet = match tokio::time::timeout(remaining, packet_stream.read_packet()).await {
            Ok(Ok(packet)) => packet,
            Ok(Err(err)) => return Err(map_packet_error(err)),
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "Timed out waiting for login start during limbo verification",
                ))
            }
        };

        append_raw_packet(capture, &packet)?;

        let packet_id = read_packet_id(packet.bytes())?;
        trace!("Captured login packet id={} during verification", packet_id);
        if packet_id == 0 {
            return Ok(());
        }
    }
}

async fn hold_connection(
    packet_stream: &mut PacketStream<BufferedStream<'_>>,
    hold: Duration,
    capture: &mut Vec<u8>,
) -> io::Result<()> {
    let hold_sleep = sleep(hold);
    tokio::pin!(hold_sleep);
    loop {
        tokio::select! {
            _ = &mut hold_sleep => {
                return Ok(());
            }
            read_res = packet_stream.read_packet() => {
                match read_res {
                    Ok(packet) => {
                        append_raw_packet(capture, &packet)?;
                    }
                    Err(err) => {
                        return Err(map_packet_error(err));
                    }
                }
            }
        }
    }
}

fn read_packet_id(packet: &[u8]) -> io::Result<i32> {
    let mut reader = BinaryReader::new(packet);
    reader
        .read::<VarInt>()
        .map(|v| v.inner())
        .map_err(map_reader_error)
}

fn append_raw_packet(buffer: &mut Vec<u8>, packet: &RawPacket) -> io::Result<()> {
    let payload = packet.bytes();
    let length = payload.len();
    let mut varint_buf = encode_varint(length);
    buffer.append(&mut varint_buf);
    buffer.extend_from_slice(payload);
    Ok(())
}

fn encode_varint(value: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(5);
    let mut val = value as u32;
    loop {
        if (val & !0x7F) == 0 {
            buf.push(val as u8);
            break;
        } else {
            buf.push(((val & 0x7F) | 0x80) as u8);
            val >>= 7;
        }
    }
    buf
}

fn try_parse_packet_header(data: &[u8]) -> io::Result<Option<(usize, usize)>> {
    let mut length = 0usize;
    let mut num_read = 0usize;

    for byte in data.iter().copied() {
        let value = (byte & 0x7F) as usize;
        length |= value << (7 * num_read);
        num_read += 1;

        if num_read > 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Handshake length varint too long",
            ));
        }

        if byte & 0x80 == 0 {
            return Ok(Some((length, num_read)));
        }
    }

    Ok(None)
}

struct BufferedStream<'a> {
    stream: &'a mut TcpStream,
    buffer: Vec<u8>,
    position: usize,
}

impl<'a> BufferedStream<'a> {
    fn new(stream: &'a mut TcpStream, buffer: Vec<u8>) -> Self {
        Self {
            stream,
            buffer,
            position: 0,
        }
    }
}

impl AsyncRead for BufferedStream<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.position < self.buffer.len() {
            let remaining = self.buffer.len() - self.position;
            let to_copy = remaining.min(buf.remaining());
            if to_copy > 0 {
                buf.put_slice(&self.buffer[self.position..self.position + to_copy]);
                self.position += to_copy;
                return Poll::Ready(Ok(()));
            }
        }
        Pin::new(&mut *self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for BufferedStream<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.stream).poll_write(cx, data)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.stream).poll_shutdown(cx)
    }
}

impl Unpin for BufferedStream<'_> {}

fn map_packet_error(err: PacketStreamError) -> io::Error {
    match err {
        PacketStreamError::Io(io_err) => io_err,
        other => io::Error::new(io::ErrorKind::InvalidData, other),
    }
}

fn map_reader_error(err: BinaryReaderError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err)
}

async fn with_timeout<'a, T>(
    fut: impl Future<Output = io::Result<T>> + Send + 'a,
    timeout: Duration,
) -> io::Result<T> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(err)) => Err(err),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Timed out waiting for handshake data",
        )),
    }
}
