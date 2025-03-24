mod proxy;
mod config_loader;
mod target_pinger;
mod forwarding;

// add the pinger module

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use crate::proxy::TcpProxy;
use crate::config_loader::BIND_ADDRESS;

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
    std::thread::spawn(|| {
        loop {
            config_loader::update_proxies();
            std::thread::sleep(Duration::from_secs(10));
        }
    });

    // Spawn the background pinger thread.
    std::thread::spawn(|| {
        target_pinger::background_pinger();
    });

    // Start the TCP proxy.
    match TcpProxy::new() {
        Ok(proxy) => {
            log::info!("TCP Proxy started successfully.");
            proxy.forward_thread.join().unwrap();
        }
        Err(e) => log::error!("Failed to start TCP Proxy: {}", e),
    }

    Ok(())
}
