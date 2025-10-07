mod config_loader;
mod forwarding;
mod metrics;
mod proxy;
mod target_pinger;

use crate::config_loader::{BIND_ADDRESS, METRICS_ADDRESS};
use crate::proxy::TcpProxy;
use std::thread;
use std::time::Duration;
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Load configuration initially.
    config_loader::update_proxies();
    let bind_addr = *BIND_ADDRESS.lock().unwrap();
    if let Some(addr) = *METRICS_ADDRESS.lock().unwrap() {
        metrics::spawn_metrics_server(addr);
    }
    println!("█▀▄▀█ █ █▄░█ █▀▀ █▀ █░█ █ █▀▀ █░░ █▀▄ ░ ▀▄▀ █▄█ ▀█");
    println!("█░▀░█ █ █░▀█ ██▄ ▄█ █▀█ █ ██▄ █▄▄ █▄▀ ▄ █░█ ░█░ █▄");
    println!("// Started on {}", bind_addr);
    println!("// On target enable receive proxy-protocol v2.");
    println!("//////////////////////////////////////////////////");
    // Spawn a background thread to update configuration periodically.
    thread::spawn(|| loop {
        config_loader::update_proxies();
        thread::sleep(Duration::from_secs(5));
    });

    // Spawn the background pinger thread.
    thread::spawn(|| {
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
