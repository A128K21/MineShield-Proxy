mod proxy;
mod logger; // if you have custom logger code; otherwise you can remove this
mod update_service;

use std::time::Duration;
use crate::proxy::TcpProxy;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the logger
    env_logger::init();

    // Print banner and startup messages
    println!("█▀▄▀█ █ █▄░█ █▀▀ █▀ █░█ █ █▀▀ █░░ █▀▄ ░ ▀▄▀ █▄█ ▀█");
    println!("█░▀░█ █ █░▀█ ██▄ ▄█ █▀█ █ ██▄ █▄▄ █▄▀ ▄ █░█ ░█░ █▄");
    println!("// Started on {}", "0.0.0.0:25565");
    println!("// On target enable receive proxy-protocol v2.");
    println!("//////////////////////////////////////////////////");

    // Load the configuration initially.
    update_service::update_proxies();

    // Spawn a background thread to update the configuration periodically.
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
            // Block until the proxy thread terminates.
            proxy.forward_thread.join().unwrap();
        }
        Err(e) => {
            log::error!("Failed to start TCP Proxy: {}", e);
        }
    }

    Ok(())
}
