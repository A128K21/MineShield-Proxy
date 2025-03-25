// ===========================================
// Imports
// ===========================================
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, TcpStream};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use lazy_static::lazy_static;
use log::error;
use crate::config_loader::{RedirectionConfig, resolve};
use crate::send_ntfy_notification;

// ===========================================
// Global State: Packet Count Rate Limiting
// ===========================================
// (domain, source_ip) -> (timestamp_in_seconds, packet_count_this_second)
lazy_static! {
    static ref DOMAIN_SRC_PACKET_COUNT: Mutex<HashMap<(String, IpAddr), (u64, usize)>> = Mutex::new(HashMap::new());
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
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut map = DOMAIN_SRC_PACKET_COUNT.lock().unwrap();
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
// Forwarding Loop
// ===========================================
/// Forwards data from `from` to `to` in a loop, checking the packet limit each time.
pub(crate) fn forward_loop(
    mut from: TcpStream,
    mut to: TcpStream,
    domain: String,
    src_ip: IpAddr,
    client_to_server: bool,
    tag: &str,
) {
    let mut buf = [0u8; 2048];
    loop {
        match from.read(&mut buf) {
            Ok(n) if n > 0 => {
                if client_to_server {
                    if let Some(cfg) = resolve(&domain) {
                        if !check_packet_limit(&domain, src_ip, &cfg) {
                            let err_msg = format!(
                                "Too many packets to domain '{}' from IP '{}' mitigating potential attack, blocking ip for 300 secs", domain, src_ip
                            );
                            error!("{}", err_msg);
                            crate::proxy::block_ip(src_ip);
                            send_ntfy_notification(&err_msg);
                            break;
                        }
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
}
