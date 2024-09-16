use std::collections::HashMap;
use std::net::{ToSocketAddrs, Ipv4Addr};
use std::sync::Mutex;
use lazy_static::lazy_static;
use mysql_async::{Pool, Row};
use mysql_async::prelude::*;

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
    let redirections = result.into_iter().filter_map(|row| {
        // Try to parse the row, log errors and continue if parsing fails
        match mysql_async::from_row_opt::<(String, String)>(row) {
            Ok((incoming_domain, target_ip)) => Some((incoming_domain, target_ip)),
            Err(e) => {
                println!("Error parsing row: {}", e);
                None // Skip this row if parsing fails
            }
        }
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
    let url = "mysql://root:mineshieldat2024@78.47.174.171:3306/mineshield";
    let pool = Pool::new(url);
    match fetch_domain_redirections(&pool).await {
        Ok(redirections) => {
            let mut new_map = HashMap::new();
            for (incoming_domain, target_ip) in redirections {
                match parse_target(&target_ip) {
                    Ok((ipv4, port)) => {
                        new_map.insert(incoming_domain, (ipv4, port));
                    },
                    Err(e) => {
                        // Log the error and skip the problematic row
                        println!("Error parsing target {}: {}", target_ip, e);
                    }
                }
            }
            let mut map = PROXY_MAP.lock().unwrap();
            *map = new_map;
        },
        Err(e) => println!("Failed to fetch redirections: {}", e),
    }
}

pub fn resolve(domain: String) -> Option<(Ipv4Addr, u16)> {
    let map = PROXY_MAP.lock().unwrap();
    map.get(&domain).cloned()
}
