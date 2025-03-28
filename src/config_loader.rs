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
    /// Number of threads to be used by the proxy (set at startup)
    pub static ref PROXY_THREADS: Mutex<usize> = Mutex::new(4);
    /// Bind address for the proxy (loaded from config)
    pub static ref BIND_ADDRESS: Mutex<SocketAddr> = Mutex::new("0.0.0.0:25565".parse().unwrap());
    /// ntfy URL composed from ntfy_server and ntfy_topic in the config.
    pub static ref NTFY_URL: Mutex<String> = Mutex::new(String::new());
    /// Debug flag read from the config.
    pub static ref DEBUG: Mutex<bool> = Mutex::new(false);
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
    pub ntfy_server: String,
    pub ntfy_topic: String,
    /// Debug flag.
    #[serde(default)]
    pub debug: bool,
    /// List of per‑domain redirections
    pub redirections: Vec<Redirection>,
}

/// The new Redirection definition from your config
#[derive(Debug, Deserialize)]
pub struct Redirection {
    pub incoming_domain: String,
    pub target: String,
    /// Maximum connections per second allowed from a single source.
    #[serde(default)]
    pub max_connections_per_second: usize,
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
    pub max_connections_per_second: usize,
    pub max_packet_per_second: usize,
    pub max_ping_response_per_second: usize,
}

// ---------- Defaults ----------

fn default_proxy_threads() -> usize {
    4
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

    // 3) Update ntfy URL from ntfy_server and ntfy_topic.
    {
        let mut ntfy_url = NTFY_URL.lock().unwrap();
        if config.ntfy_server.trim().is_empty() || config.ntfy_topic.trim().is_empty() {
            *ntfy_url = String::new();
        } else {
            *ntfy_url = format!("{}/{}", config.ntfy_server.trim(), config.ntfy_topic.trim());
        }
    }

    // 4) Update debug flag
    {
        let mut debug_flag = DEBUG.lock().unwrap();
        *debug_flag = config.debug;
    }

    // 5) Parse and store redirections in a map
    let mut new_map = HashMap::new();
    for rd in config.redirections {
        match parse_target(&rd.target) {
            Ok((ip, port)) => {
                let rcfg = RedirectionConfig {
                    ip,
                    port,
                    max_connections_per_second: rd.max_connections_per_second,
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

# ntfy integration "ntfy.sh" "xy-topic" leave blank if disabled.
ntfy_server: ""
ntfy_topic: ""

# Debug messages for proxy development
debug: false

# Where should we route incoming connections?
redirections:
  - incoming_domain: "localhost"
    target: "127.0.0.1:25577"
    # Max packets/second before kicking. 0 = none
    max_packet_per_second: 0
    # Max ping responses/second from cache
    max_ping_response_per_second: 0
    # Maximum connections per second from a single source. 0 = unlimited
    max_connections_per_second: 0

  - incoming_domain: "example.com"
    target: "target.local:25678"
    # Max packets/second before kicking. 0 = none
    max_packet_per_second: 100
    # Max ping responses/second from cache
    max_ping_response_per_second: 100
    # Maximum connections per second from a single source. 0 = unlimited
    max_connections_per_second: 5

"#.to_string()
}
