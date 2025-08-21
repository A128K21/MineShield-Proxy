// ===========================================
// Imports
// ===========================================
use crate::config_loader::RedirectionConfig;
use crate::send_ntfy_notification;
use dashmap::DashMap;
use lazy_static::lazy_static;
use log::error;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ===========================================
// Global State: Packet Count Rate Limiting
// ===========================================
// (domain, source_ip) -> (timestamp_in_seconds, packet_count_this_second)
lazy_static! {
    static ref DOMAIN_SRC_PACKET_COUNT: DashMap<(String, IpAddr), (u64, usize)> = DashMap::new();
    // The cleanup thread periodically prunes stale entries so the map's size
    // tracks only active traffic instead of growing without bound.
}

// ===========================================
// Helper Function: Packet Limit Check
// ===========================================
/// Check or increment the per‑domain + per‑source packet count.
/// Returns `true` if allowed, `false` if the limit is exceeded.
fn check_packet_limit(domain: &str, src_ip: IpAddr, cfg: &RedirectionConfig) -> bool {
    let limit = cfg.max_packet_per_second;
    if limit == 0 {
        return true; // unlimited
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut entry = DOMAIN_SRC_PACKET_COUNT
        .entry((domain.to_string(), src_ip))
        .or_insert((now, 0));
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
// Forwarding Loop
// ===========================================
/// Forwards data from `from` to `to` in a loop, checking the packet limit each time.
pub(crate) fn forward_loop(
    mut from: TcpStream,
    mut to: TcpStream,
    domain: String,
    cfg: Arc<RedirectionConfig>,
    src_ip: IpAddr,
    src_addr: SocketAddr,
    client_to_server: bool,
    tag: &str,
) {
    let mut buf = [0u8; 2048];
    loop {
        match from.read(&mut buf) {
            Ok(n) if n > 0 => {
                if client_to_server {
                    if !check_packet_limit(&domain, src_ip, &cfg) {
                        let err_msg = format!(
                            "Too many packets to domain '{}' from IP '{}' mitigating potential attack, blocking ip for 300 secs",
                            domain, src_ip
                        );
                        error!("{}", err_msg);
                        crate::proxy::block_ip(src_ip);
                        send_ntfy_notification(&err_msg);
                        break;
                    }
                }
                if to.write_all(&buf[..n]).is_err() {
                    error!("{} - write error", tag);
                    break;
                }
            }
            Ok(_) => break, // 0 bytes read => EOF
            Err(e) => {
                error!("{} - read error: {}", tag, e);
                break;
            }
        }
    }
    let _ = to.shutdown(Shutdown::Write);
    crate::proxy::finish_connection(src_addr);
}

// Start background cleanup for packet rate-limit map
pub fn start_packet_cleanup_thread() {
    thread::spawn(|| loop {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // Retain counters from the current and previous second so ongoing
        // traffic isn't disturbed while still pruning old entries.
        DOMAIN_SRC_PACKET_COUNT.retain(|_, v| v.0 + 1 >= now);
        thread::sleep(Duration::from_secs(1));
    });
}
