use std::collections::HashMap;
use std::ffi::c_int;
use std::net::{ToSocketAddrs, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::fs::File;
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};
use lazy_static::lazy_static;
use serde::Deserialize;

lazy_static! {
    // Mapping from incoming domain to target (IP and port)
    pub static ref PROXY_MAP: Mutex<HashMap<String, (Ipv4Addr, u16)>> = Mutex::new(HashMap::new());
    // For each target, track (timestamp in seconds, count of connection attempts in that second)
    static ref ACTIVE_TARGET_CONNECTIONS: Mutex<HashMap<String, (u64, usize)>> = Mutex::new(HashMap::new());
    // Overload configuration (loaded from the config file)
    static ref OVERLOAD_CONFIG: Mutex<PreventTargetOverloadConfig> = Mutex::new(PreventTargetOverloadConfig::default());
    // Number of threads to be used by the proxy (set at startup)
    pub static ref PROXY_THREADS: Mutex<usize> = Mutex::new(default_proxy_threads());
    // Bind address for the proxy (loaded from config)
    pub static ref BIND_ADDRESS: Mutex<SocketAddr> = Mutex::new(default_bind_address());
}

fn default_proxy_threads() -> usize {
    4
}

fn default_rate_limit() -> usize {
    10
}

fn default_bind_address() -> SocketAddr {
    "0.0.0.0:25565".parse().unwrap()
}

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Where should the proxy listen for connections?
    #[serde(rename = "bind-address")]
    pub bind_address: String,
    /// Settings for target overload prevention.
    #[serde(default)]
    pub prevent_target_overload: PreventTargetOverloadConfig,
    /// Number of threads to use for the proxy. (Only read at startup.)
    #[serde(default = "default_proxy_threads")]
    pub proxy_threads: usize,
    pub redirections: Vec<Redirection>,
}

#[derive(Default, Debug, Deserialize, Clone)]
pub struct PreventTargetOverloadConfig {
    /// Whether to enable target overload prevention.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum number of connection attempts allowed per target per second.
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_target: usize,
}

#[derive(Debug, Deserialize)]
pub struct Redirection {
    pub incoming_domain: String,
    pub target: String,
}

/// A guard that is returned when a connection is allowed.
/// (In this per‑second rate limiter, dropping the guard does nothing.)
pub struct ConnectionGuard {
    pub target: String,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        // Do nothing—the per‑second rate limiter resets automatically.
    }
}

/// Attempts to register a connection for a given target.
/// Returns Some(Arc<ConnectionGuard>) if allowed, or None if the rate limit is exceeded.
pub fn try_register_connection(target: &str) -> Option<std::sync::Arc<ConnectionGuard>> {
    let mut connections = ACTIVE_TARGET_CONNECTIONS.lock().unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let overload = OVERLOAD_CONFIG.lock().unwrap();
    if !overload.enabled {
        // If overload prevention is disabled, always allow.
        return Some(std::sync::Arc::new(ConnectionGuard {
            target: target.to_string(),
        }));
    }
    // Get or create an entry for the target.
    let entry = connections.entry(target.to_string()).or_insert((now, 0));
    if entry.0 == now {
        if entry.1 >= overload.rate_limit_per_target {
            return None;
        } else {
            entry.1 += 1;
        }
    } else {
        // New second: reset counter.
        *entry = (now, 1);
    }
    Some(std::sync::Arc::new(ConnectionGuard {
        target: target.to_string(),
    }))
}

fn convert_to_ipv4(addr: &str) -> Result<Ipv4Addr, String> {
    // If already an IPv4 address, return it.
    if let Ok(ip) = addr.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    // Otherwise, resolve the domain.
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
/// a default configuration (with inline comments) is generated and written to the file.
pub fn update_proxies_from_config(config_path: &str) {
    let mut contents = String::new();

    match File::open(config_path) {
        Ok(mut file) => {
            file.read_to_string(&mut contents)
                .expect("Unable to read config file");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("Config file not found. Creating a default config file.");
            contents = r#"# Default configuration for proxy redirections.
# Where should the proxy listen for connections?
bind-address: "0.0.0.0:25565"

prevent_target_overload:
  # Set `enabled` to true to enable target overload prevention.
  enabled: false
  # The `rate_limit_per_target` field specifies the maximum number of connection attempts allowed per target per second.
  rate_limit_per_target: 10

# Number of threads to use for the proxy (only used at startup)
proxy_threads: 4
# Where should we route incoming connections?
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
        }
        Err(e) => {
            panic!("Error opening config file: {}", e);
        }
    }

    let config: Config = serde_yaml::from_str(&contents)
        .expect("Failed to parse YAML");

    // Update global bind address
    {
        let mut bind_addr = BIND_ADDRESS.lock().unwrap();
        *bind_addr = config.bind_address.parse().expect("Invalid bind address in config");
    }

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
            }
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
