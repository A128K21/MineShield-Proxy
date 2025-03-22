mod proxy;
mod update_service;
mod pinger; // add the pinger module

use std::time::Duration;
use crate::proxy::TcpProxy;
use crate::update_service::BIND_ADDRESS;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();


    // Load configuration initially.
    update_service::update_proxies();
    let bind_addr = *BIND_ADDRESS.lock().unwrap();
    println!("█▀▄▀█ █ █▄░█ █▀▀ █▀ █░█ █ █▀▀ █░░ █▀▄ ░ ▀▄▀ █▄█ ▀█");
    println!("█░▀░█ █ █░▀█ ██▄ ▄█ █▀█ █ ██▄ █▄▄ █▄▀ ▄ █░█ ░█░ █▄");
    println!("// Started on {}", bind_addr);
    println!("// On target enable receive proxy-protocol v2.");
    println!("//////////////////////////////////////////////////");
    // Spawn a background thread to update configuration periodically.
    std::thread::spawn(|| {
        loop {
            update_service::update_proxies();
            std::thread::sleep(Duration::from_secs(10));
        }
    });

    // Spawn the background pinger thread.
    std::thread::spawn(|| {
        pinger::background_pinger();
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
