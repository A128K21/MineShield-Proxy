mod proxy;
mod logger;


use std::net::SocketAddr;
use std::time::Duration;
use crate::proxy::TcpProxy;


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let proxy = TcpProxy::new();

    loop {
        // Your code here
        // Use your proxy here

        // Sleep for 10 seconds
        std::thread::sleep(std::time::Duration::from_secs(10));
    }

    Ok(())
}