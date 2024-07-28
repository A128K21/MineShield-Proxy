mod proxy;
mod logger;
mod update_service;


use std::net::SocketAddr;
use std::time::Duration;
use crate::proxy::TcpProxy;


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    TcpProxy::new();
    loop {
        // Your code here
        // Use your proxy here
        update_service::update_proxies().await;
        // Sleep for 10 seconds
        std::thread::sleep(Duration::from_secs(1));

        if let Some((ip, port)) = update_service::resolve("localhost".parse().unwrap()) {
            println!("Resolved localhost to {}:{}", ip, port);
        }
        if let Some((ip, port)) = update_service::resolve("google.com".parse().unwrap()) {
            println!("Resolved google.com to {}:{}", ip, port);
        }
    }

    Ok(())
}