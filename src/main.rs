mod config_loader;
mod forwarding;
mod proxy;
mod target_pinger;

// add the pinger module

use crate::config_loader::{BIND_ADDRESS, NTFY_URL};
use crate::proxy::TcpProxy;
use lazy_static::lazy_static;
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
lazy_static! {
    // Global last notification time for rate-limiting ntfy messages (in seconds).
    pub(crate) static ref LAST_NTFY_TIME: Mutex<u64> = Mutex::new(0);
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Load configuration initially.
    config_loader::update_proxies();
    let bind_addr = *BIND_ADDRESS.lock().unwrap();
    println!("█▀▄▀█ █ █▄░█ █▀▀ █▀ █░█ █ █▀▀ █░░ █▀▄ ░ ▀▄▀ █▄█ ▀█");
    println!("█░▀░█ █ █░▀█ ██▄ ▄█ █▀█ █ ██▄ █▄▄ █▄▀ ▄ █░█ ░█░ █▄");
    println!("// Started on {}", bind_addr);
    println!("// On target enable receive proxy-protocol v2.");
    println!("//////////////////////////////////////////////////");
    // Spawn a background thread to update configuration periodically.
    std::thread::spawn(|| loop {
        config_loader::update_proxies();
        std::thread::sleep(Duration::from_secs(5));
    });

    // Spawn the background pinger thread.
    std::thread::spawn(|| {
        target_pinger::background_pinger();
    });

    // Start the TCP proxy.
    match TcpProxy::new().await {
        Ok(proxy) => {
            log::info!("TCP Proxy started successfully.");
            proxy.forward_task.await.unwrap();
        }
        Err(e) => log::error!("Failed to start TCP Proxy: {}", e),
    }

    Ok(())
}
// ===========================================
// NTFY Notification with Rate Limiting
// ===========================================
pub fn send_ntfy_notification(message: &str) {
    let message_owned = message.to_owned();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut last_time = crate::LAST_NTFY_TIME.lock().unwrap();
    if now <= *last_time {
        // Already sent a notification in the current second, so skip.
        return;
    }
    *last_time = now;

    // Retrieve the ntfy URL from config_loader
    let ntfy_url = {
        let ntfy_url_lock = NTFY_URL.lock().unwrap();
        ntfy_url_lock.clone()
    };
    // If ntfy_url is empty, skip sending a notification.
    if ntfy_url.is_empty() {
        return;
    }

    thread::spawn(move || {
        if let Err(e) = Command::new("curl")
            .arg("-d")
            .arg(format!("[MineShield-Proxy] {}", message_owned))
            .arg(ntfy_url)
            .status()
        {
            eprintln!("Failed to send ntfy notification: {}", e);
        }
    });
}
