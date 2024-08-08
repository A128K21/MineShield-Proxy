mod proxy;
mod logger;
mod update_service;


use std::net::SocketAddr;
use std::time::Duration;
use crate::proxy::TcpProxy;


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    println!("█▀▄▀█ █ █▄░█ █▀▀ █▀ █░█ █ █▀▀ █░░ █▀▄ ░ ▀▄▀ █▄█ ▀█");
    println!("█░▀░█ █ █░▀█ ██▄ ▄█ █▀█ █ ██▄ █▄▄ █▄▀ ▄ █░█ ░█░ █▄");
    println!("// Started on {}", "0.0.0.0:25565");
    println!("//////////////////////////////////////////////////");
    TcpProxy::new();
    loop {
        // Your code here
        // Use your proxy here
        update_service::update_proxies().await;
        // Sleep for 10 seconds
        std::thread::sleep(Duration::from_secs(1));

    }

    Ok(())
}