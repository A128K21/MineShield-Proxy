use log::info;
use std::collections::HashMap;
use std::net::{ToSocketAddrs, Ipv4Addr, SocketAddrV4};
use std::sync::Mutex;
use tokio::time::{self, Duration};
use lazy_static::lazy_static;
use mysql_async::{Pool, Row};
use mysql_async::prelude::*;
use dns_lookup::lookup_host;

lazy_static! {
    static ref PROXY_MAP: Mutex<HashMap<String, (Ipv4Addr, u16)>> = Mutex::new(HashMap::new());
}

fn convert_to_ipv4(addr: &str) -> Result<Ipv4Addr, String> {
    // Check if the address is already an IPv4 address
    if let Ok(ip) = addr.parse::<Ipv4Addr>() {
        return Ok(ip);
    }

    // Attempt to resolve the address as a domain name and filter for IPv4 addresses
    let socket_addrs = (addr, 0).to_socket_addrs()
        .map_err(|e| format!("Failed to resolve domain name: {}", e))?;

    for socket_addr in socket_addrs {
        if let std::net::IpAddr::V4(ipv4_addr) = socket_addr.ip() {
            return Ok(ipv4_addr);
        }
    }

    Err("No IPv4 address found for the domain".to_string())
}

async fn fetch_domain_redirections(pool: &Pool) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    let mut conn = pool.get_conn().await?;
    let result: Vec<Row> = conn.query("SELECT incoming_domain, target_ip FROM domain_redirections").await?;
    let redirections = result.into_iter().map(|row| {
        let (incoming_domain, target_ip): (String, String) = mysql_async::from_row(row);
        (incoming_domain, target_ip)
    }).collect();
    Ok(redirections)
}

fn parse_target(target: &str) -> Result<(Ipv4Addr, u16), String> {
    let parts: Vec<&str> = target.split(':').collect();
    if parts.len() != 2 {
        return Err("Invalid target format".to_string());
    }
    let addr = parts[0];
    let port: u16 = parts[1].parse().map_err(|_| "Invalid port".to_string())?;
    let ipv4 = convert_to_ipv4(addr)?;
    Ok((ipv4, port))
}

pub(crate) async fn update_proxies() {
    // println!("Attempting mysql connection...");
    let url = "mysql://root:mineshieldat2024@10.0.0.3:3306/mineshield";
    let pool = Pool::new(url);
    // println!("Connected to mysql server!");
    match fetch_domain_redirections(&pool).await {
        Ok(redirections) => {
            let mut new_map = HashMap::new();
            for (incoming_domain, target_ip) in redirections {
                match parse_target(&target_ip) {
                    Ok((ipv4, port)) => {
                        new_map.insert(incoming_domain, (ipv4, port));
                    },
                    Err(e) => println!("Error parsing target {}: {}", target_ip, e),
                }
            }
            let mut map = PROXY_MAP.lock().unwrap();
            *map = new_map;
            // println!("Updated proxies");
        },
        Err(e) => println!("Failed to fetch redirections: {}", e),
    }
}

pub fn resolve(domain: String) -> Option<(Ipv4Addr, u16)> {
    let map = PROXY_MAP.lock().unwrap();
    map.get(&domain).cloned()
}