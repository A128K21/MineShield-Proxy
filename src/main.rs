mod proxy;
mod update_service;

use std::time::Duration;
use crate::proxy::TcpProxy;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    println!("█▀▄▀█ █ █▄░█ █▀▀ █▀ █░█ █ █▀▀ █░░ █▀▄ ░ ▀▄▀ █▄█ ▀█");
    println!("█░▀░█ █ █░▀█ ██▄ ▄█ █▀█ █ ██▄ █▄▄ █▄▀ ▄ █░█ ░█░ █▄");
    println!("// Started on {}", "0.0.0.0:25565");
    println!("// On target enable receive proxy-protocol v2.");
    println!("//////////////////////////////////////////////////");

    // Load the configuration initially.
    update_service::update_proxies();

    // Spawn a background thread to update configuration periodically.
    std::thread::spawn(|| {
        loop {
            update_service::update_proxies();
            std::thread::sleep(Duration::from_secs(10));
        }
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
