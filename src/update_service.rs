use std::collections::HashMap;
use std::net::{ToSocketAddrs, Ipv4Addr};
use std::sync::Mutex;
use std::fs::File;
use std::io::{Read, Write};
use lazy_static::lazy_static;
use serde::Deserialize;

lazy_static! {
    // Mapping from incoming domain to target (IP and port)
    static ref PROXY_MAP: Mutex<HashMap<String, (Ipv4Addr, u16)>> = Mutex::new(HashMap::new());
    // Active connection counts per target domain
    static ref ACTIVE_TARGET_CONNECTIONS: Mutex<HashMap<String, usize>> = Mutex::new(HashMap::new());
    // Overload configuration (loaded from the config file)
    static ref OVERLOAD_CONFIG: Mutex<PreventTargetOverloadConfig> = Mutex::new(PreventTargetOverloadConfig::default());
    // Number of threads to be used by the proxy (set at startup)
    pub static ref PROXY_THREADS: Mutex<usize> = Mutex::new(default_proxy_threads());
}

fn default_proxy_threads() -> usize {
    4
}

fn default_rate_limit() -> usize {
    10
}

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Settings for target overload prevention.
    #[serde(default)]
    pub prevent_target_overload: PreventTargetOverloadConfig,
    /// Number of threads to use for the proxy. This is only read at startup.
    #[serde(default = "default_proxy_threads")]
    pub proxy_threads: usize,
    pub redirections: Vec<Redirection>,
}

#[derive(Default, Debug, Deserialize, Clone)]
pub struct PreventTargetOverloadConfig {
    /// Whether to enable target overload prevention.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of requests allowed per target.
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_target: usize,
}

#[derive(Debug, Deserialize)]
pub struct Redirection {
    pub incoming_domain: String,
    pub target: String,
}

/// A guard that holds registration of one connection for a given target.
/// When the guard is dropped, the active connection count is decremented.
pub struct ConnectionGuard {
    pub target: String,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        let mut connections = ACTIVE_TARGET_CONNECTIONS.lock().unwrap();
        if let Some(count) = connections.get_mut(&self.target) {
            if *count > 0 {
                *count -= 1;
            }
        }
    }
}

/// Try to register a connection for a given target.
/// Returns Some(Arc<ConnectionGuard>) if allowed, or None if the rate limit is exceeded.
pub fn try_register_connection(target: &str) -> Option<std::sync::Arc<ConnectionGuard>> {
    let mut connections = ACTIVE_TARGET_CONNECTIONS.lock().unwrap();
    let count = connections.entry(target.to_string()).or_insert(0);
    let overload = OVERLOAD_CONFIG.lock().unwrap();
    if overload.enabled && *count >= overload.rate_limit_per_target {
        return None;
    }
    *count += 1;
    drop(connections);
    Some(std::sync::Arc::new(ConnectionGuard {
        target: target.to_string(),
    }))
}

fn convert_to_ipv4(addr: &str) -> Result<Ipv4Addr, String> {
    // If already an IPv4 address, return it.
    if let Ok(ip) = addr.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    // Otherwise, resolve the domain name.
    let socket_addrs = (addr, 0)
        .to_socket_addrs()
        .map_err(|e| format!("Failed to resolve domain name: {}", e))?;
    for socket_addr in socket_addrs {
        if let std::net::IpAddr::V4(ipv4_addr) = socket_addr.ip() {
            return Ok(ipv4_addr);
        }
    }
    Err("No IPv4 address found for the domain".to_string())
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

/// Loads the configuration from the given path. If the file does not exist,
/// a default configuration (with comments) is generated and written to the file.
pub fn update_proxies_from_config(config_path: &str) {
    let mut contents = String::new();

    match File::open(config_path) {
        Ok(mut file) => {
            file.read_to_string(&mut contents)
                .expect("Unable to read config file");
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("Config file not found. Creating a default config file.");
            contents = r#"# Default configuration for proxy redirections.
prevent_target_overload:
  # Set `enabled` to true to enable target overload prevention.
  enabled: false
  # The `rate_limit_per_target` field specifies the maximum number of requests allowed per target.
  rate_limit_per_target: 10

# Number of threads to use for the proxy (only used at startup)
proxy_threads: 4

redirections:
  - incoming_domain: "example.com"
    target: "192.168.1.100:8080"
  - incoming_domain: "test.com"
    target: "some.domain.com:9090"
"#
                .to_string();
            let mut file = File::create(config_path)
                .expect("Unable to create default config file");
            file.write_all(contents.as_bytes())
                .expect("Unable to write default config file");
        },
        Err(e) => {
            panic!("Error opening config file: {}", e);
        }
    }

    let config: Config = serde_yaml::from_str(&contents)
        .expect("Failed to parse YAML");



    {
        let mut overload = OVERLOAD_CONFIG.lock().unwrap();
        *overload = config.prevent_target_overload.clone();
    }

    {
        let mut threads = PROXY_THREADS.lock().unwrap();
        *threads = config.proxy_threads;
    }

    let mut new_map = HashMap::new();
    for redirection in config.redirections {
        match parse_target(&redirection.target) {
            Ok((ipv4, port)) => {
                new_map.insert(redirection.incoming_domain, (ipv4, port));
            },
            Err(e) => {
                println!("Error parsing target {}: {}", redirection.target, e);
            }
        }
    }

    let mut map = PROXY_MAP.lock().unwrap();
    *map = new_map;
}

/// Synchronous wrapper to update proxies from the default config file ("config.yml")
pub fn update_proxies() {
    update_proxies_from_config("config.yml");
}

/// Resolves a given domain to its proxy mapping.
pub fn resolve(domain: String) -> Option<(Ipv4Addr, u16)> {
    let map = PROXY_MAP.lock().unwrap();
    map.get(&domain).cloned()
}
