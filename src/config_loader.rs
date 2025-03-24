use std::collections::HashMap;
use std::net::{ToSocketAddrs, Ipv4Addr, SocketAddr};
use std::sync::Mutex;
use std::fs::File;
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};
use lazy_static::lazy_static;
use log::error;
use serde::Deserialize;

// ---------- Global locks and references ----------

lazy_static! {
    /// For each domain, we store its RedirectionConfig
    pub static ref REDIRECTION_MAP: Mutex<HashMap<String, RedirectionConfig>> = Mutex::new(HashMap::new());
    /// For each domain, track (timestamp in seconds, count of connection attempts in that second)
    static ref ACTIVE_CONNECTIONS: Mutex<HashMap<String, (u64, usize)>> = Mutex::new(HashMap::new());
    /// Number of threads to be used by the proxy (set at startup)
    pub static ref PROXY_THREADS: Mutex<usize> = Mutex::new(4);
    /// Bind address for the proxy (loaded from config)
    pub static ref BIND_ADDRESS: Mutex<SocketAddr> = Mutex::new("0.0.0.0:25565".parse().unwrap());
}

// ---------- Data structures ----------

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Where should the proxy listen for connections?
    #[serde(rename = "bind-address")]
    pub bind_address: String,
    /// Number of threads to use for the proxy. (Only read at startup.)
    #[serde(default = "default_proxy_threads")]
    pub proxy_threads: usize,
    /// List of per‑domain redirections
    pub redirections: Vec<Redirection>,
}

/// The new Redirection definition from your config
#[derive(Debug, Deserialize)]
pub struct Redirection {
    pub incoming_domain: String,
    pub target: String,
    /// Maximum connections forwarded per second. 0 = unlimited.
    #[serde(default)]
    pub rate_limit: usize,
    /// Maximum connections per second allowed from a single source.
    #[serde(default)]
    pub max_connections_per_second: usize,
    /// Should we check encryption? (Placeholder - logic not shown here)
    #[serde(default)]
    pub encryption_check: bool,
    /// Max packets/second before kicking. 0 = none.
    #[serde(default)]
    pub max_packet_per_second: usize,
    /// Max ping responses/second from cache.
    #[serde(default)]
    pub max_ping_response_per_second: usize,
}

/// This is what we'll store per domain internally
#[derive(Clone, Debug)]
pub struct RedirectionConfig {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub rate_limit: usize,
    pub max_connections_per_second: usize,
    pub encryption_check: bool,
    pub max_packet_per_second: usize,
    pub max_ping_response_per_second: usize,
}

// ---------- Defaults ----------

fn default_proxy_threads() -> usize {
    4
}

// ---------- Connection Guard ----------

/// A guard that is returned when a connection is allowed.
/// (In this per‑second rate limiter, dropping the guard does nothing.)
pub struct ConnectionGuard {
    pub domain: String,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        // Do nothing—the per‑second rate limiter resets automatically.
    }
}

// ---------- Helpers to load config ----------

/// Resolves a host (like "127.0.0.1" or "example.com") to an `Ipv4Addr`.
fn convert_to_ipv4(addr: &str) -> Result<Ipv4Addr, String> {
    // If it's already an IPv4 string, parse and return it.
    if let Ok(ip) = addr.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    // Otherwise, try DNS resolution.
    let socket_addrs = (addr, 0)
        .to_socket_addrs()
        .map_err(|e| format!("Failed to resolve '{}': {}", addr, e))?;
    for socket_addr in socket_addrs {
        if let std::net::IpAddr::V4(ipv4_addr) = socket_addr.ip() {
            return Ok(ipv4_addr);
        }
    }
    Err(format!("No IPv4 address found for '{}'", addr))
}

fn parse_target(target: &str) -> Result<(Ipv4Addr, u16), String> {
    let parts: Vec<&str> = target.split(':').collect();
    if parts.len() != 2 {
        return Err("Invalid target format (expected host:port)".to_string());
    }
    let host = parts[0];
    let port: u16 = parts[1]
        .parse()
        .map_err(|_| "Invalid port in target".to_string())?;
    let ipv4 = convert_to_ipv4(host)?;
    Ok((ipv4, port))
}

// ---------- Public API ----------

/// Attempts to register a connection for the given domain. Returns a guard if allowed, None if blocked.
pub fn try_register_connection(domain: &str) -> Option<std::sync::Arc<ConnectionGuard>> {
    // Look up the domain's rate limit
    let limit_opt = {
        let map = REDIRECTION_MAP.lock().unwrap();
        map.get(domain).map(|rd| rd.rate_limit)
    };

    // If we don't know this domain or the limit is 0 => disabled => always allow
    let rate_limit = match limit_opt {
        None => return Some(std::sync::Arc::new(ConnectionGuard { domain: domain.into() })),
        Some(l) if l == 0 => {
            return Some(std::sync::Arc::new(ConnectionGuard { domain: domain.into() }));
        }
        Some(l) => l,
    };

    let mut connections = ACTIVE_CONNECTIONS.lock().unwrap();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

    let entry = connections.entry(domain.to_string()).or_insert((now, 0));
    if entry.0 == now {
        // same second
        if entry.1 >= rate_limit {
            return None;
        } else {
            entry.1 += 1;
        }
    } else {
        // new second, reset
        *entry = (now, 1);
    }
    Some(std::sync::Arc::new(ConnectionGuard { domain: domain.into() }))
}

/// Loads YAML from `config_path` and updates global settings.
pub fn update_proxies_from_config(config_path: &str) {
    let mut contents = String::new();

    match File::open(config_path) {
        Ok(mut file) => {
            file.read_to_string(&mut contents)
                .expect("Failed to read config file");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            error!("Config file not found. Creating a default config file...");
            contents = default_config();
            let mut file =
                File::create(config_path).expect("Unable to create default config file");
            file.write_all(contents.as_bytes())
                .expect("Unable to write default config file");
        }
        Err(e) => {
            panic!("Error opening config file: {}", e);
        }
    }

    let config: Config =
        serde_yaml::from_str(&contents).expect("Failed to parse YAML config");

    // 1) Update the bind address
    {
        let mut bind_addr = BIND_ADDRESS.lock().unwrap();
        *bind_addr = config
            .bind_address
            .parse()
            .expect("Invalid bind address in config");
    }

    // 2) Update proxy threads
    {
        let mut threads = PROXY_THREADS.lock().unwrap();
        *threads = config.proxy_threads;
    }

    // 3) Parse and store redirections in a map
    let mut new_map = HashMap::new();
    for rd in config.redirections {
        match parse_target(&rd.target) {
            Ok((ip, port)) => {
                let rcfg = RedirectionConfig {
                    ip,
                    port,
                    rate_limit: rd.rate_limit,
                    max_connections_per_second: rd.max_connections_per_second,
                    encryption_check: rd.encryption_check,
                    max_packet_per_second: rd.max_packet_per_second,
                    max_ping_response_per_second: rd.max_ping_response_per_second,
                };
                new_map.insert(rd.incoming_domain, rcfg);
            }
            Err(e) => {
                error!("Error parsing target '{}': {}", rd.target, e);
            }
        }
    }
    {
        let mut map = REDIRECTION_MAP.lock().unwrap();
        *map = new_map;
    }
}

/// Shortcut: updates from `config.yml`
pub fn update_proxies() {
    update_proxies_from_config("config.yml");
}

/// Resolves a given domain to an `(ip, port)`, plus all other config data.
pub fn resolve(domain: &str) -> Option<RedirectionConfig> {
    let map = REDIRECTION_MAP.lock().unwrap();
    map.get(domain).cloned()
}

// A default config, just in case the file doesn't exist.
fn default_config() -> String {
    r#"# Default configuration for proxy redirections.
# Where should the proxy listen for connections?
bind-address: "127.0.0.1:25565"

# Number of threads to use for the proxy (only used at startup)
proxy_threads: 4

# Where should we route incoming connections?
redirections:
  - incoming_domain: "localhost"
    target: "127.0.0.1:25577"
    # Should we check encryption? (Placeholder - logic not shown here)
    encryption_check: false
    # Maximum amount of connections forwarded to target per second. 0 = unlimited
    rate_limit: 0
    # Max packets/second before kicking. 0 = none
    max_packet_per_second: 0
    # Max ping responses/second from cache
    max_ping_response_per_second: 0
    # Maximum connections per second from a single source. 0 = unlimited
    max_connections_per_second: 0

  - incoming_domain: "example.com"
    target: "target.local:25678"
    # Should we check encryption? (Placeholder - logic not shown here)
    encryption_check: false
    # Maximum amount of connections forwarded to target per second. 0 = unlimited
    rate_limit: 100
    # Max packets/second before kicking. 0 = none
    max_packet_per_second: 100
    # Max ping responses/second from cache
    max_ping_response_per_second: 100
    # Maximum connections per second from a single source. 0 = unlimited
    max_connections_per_second: 5

"#.to_string()
}
