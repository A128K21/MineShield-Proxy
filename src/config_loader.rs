use dashmap::DashMap;
use lazy_static::lazy_static;
use log::error;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// ---------- Global locks and references ----------

lazy_static! {
    /// For each domain, we store its RedirectionConfig
    pub static ref REDIRECTION_MAP: DashMap<String, RedirectionConfig> = DashMap::new();
    /// Number of threads to be used by the proxy (set at startup)
    pub static ref PROXY_THREADS: Mutex<usize> = Mutex::new(4);
    /// Bind address for the proxy (loaded from config)
    pub static ref BIND_ADDRESS: Mutex<SocketAddr> = Mutex::new("0.0.0.0:25565".parse().unwrap());
    /// ntfy URL composed from ntfy_server and ntfy_topic in the config.
    pub static ref NTFY_URL: Mutex<String> = Mutex::new(String::new());
    /// Whether the Prometheus exporter should be enabled.
    pub static ref PROMETHEUS_EXPORTER_ENABLED: AtomicBool = AtomicBool::new(false);
    /// Address the Prometheus exporter should bind to.
    pub static ref PROMETHEUS_EXPORTER_BIND_ADDRESS: Mutex<SocketAddr> = Mutex::new(
        "0.0.0.0:9100".parse().unwrap()
    );
}

/// Debug flag read from the config.
pub static DEBUG: AtomicBool = AtomicBool::new(false);

// ---------- Data structures ----------

#[derive(Debug, Deserialize)]
pub struct Config {
    /// Where should the proxy listen for connections?
    #[serde(rename = "bind-address")]
    pub bind_address: String,
    /// Number of threads to use for the proxy. (Only read at startup.)
    #[serde(default = "default_proxy_threads")]
    pub proxy_threads: usize,
    #[serde(default)]
    pub ntfy_server: String,
    #[serde(default)]
    pub ntfy_topic: String,
    #[serde(default)]
    pub prometheus_exporter: PrometheusExporterConfig,
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

/// Configuration for the Prometheus exporter endpoint.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct PrometheusExporterConfig {
    pub enabled: bool,
    #[serde(rename = "bind-address")]
    pub bind_address: String,
}

impl Default for PrometheusExporterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind_address: "0.0.0.0:9100".to_string(),
        }
    }
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
            let mut file = File::create(config_path).expect("Unable to create default config file");
            file.write_all(contents.as_bytes())
                .expect("Unable to write default config file");
        }
        Err(e) => {
            panic!("Error opening config file: {}", e);
        }
    }

    let config: Config = serde_yaml::from_str(&contents).expect("Failed to parse YAML config");

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

    // 4) Update Prometheus exporter configuration
    PROMETHEUS_EXPORTER_ENABLED.store(config.prometheus_exporter.enabled, Ordering::Relaxed);
    {
        let mut exporter_bind = PROMETHEUS_EXPORTER_BIND_ADDRESS.lock().unwrap();
        *exporter_bind = config
            .prometheus_exporter
            .bind_address
            .parse()
            .expect("Invalid Prometheus exporter bind address in config");
    }

    // 5) Update debug flag
    DEBUG.store(config.debug, Ordering::Relaxed);

    // 6) Parse and store redirections in a map
    let mut domains_in_config = HashSet::new();
    for rd in config.redirections {
        let domain_key = rd.incoming_domain.to_ascii_lowercase();
        domains_in_config.insert(domain_key.clone());

        match parse_target(&rd.target) {
            Ok((ip, port)) => {
                let rcfg = RedirectionConfig {
                    ip,
                    port,
                    max_connections_per_second: rd.max_connections_per_second,
                    max_packet_per_second: rd.max_packet_per_second,
                    max_ping_response_per_second: rd.max_ping_response_per_second,
                };
                REDIRECTION_MAP.insert(domain_key, rcfg);
            }
            Err(e) => {
                error!("Error parsing target '{}': {}", rd.target, e);
                if let Some(mut existing) = REDIRECTION_MAP.get_mut(&domain_key) {
                    existing.max_connections_per_second = rd.max_connections_per_second;
                    existing.max_packet_per_second = rd.max_packet_per_second;
                    existing.max_ping_response_per_second = rd.max_ping_response_per_second;
                }
            }
        }
    }

    REDIRECTION_MAP.retain(|k, _| domains_in_config.contains(k));
}

/// Shortcut: updates from `config.yml`
pub fn update_proxies() {
    update_proxies_from_config("config.yml");
}

/// Resolves a given domain to an `(ip, port)`, plus all other config data.
pub fn resolve(domain: &str) -> Option<RedirectionConfig> {
    REDIRECTION_MAP.get(domain).map(|v| v.clone())
}

// A default config, just in case the file doesn't exist.
fn default_config() -> String {
    r#"# Default configuration for proxy redirections.
# Where should the proxy listen for connections?
bind-address: "127.0.0.1:25565"

# Number of threads for listener and forwarding pools (only used at startup)
proxy_threads: 4

# Prometheus exporter configuration
prometheus_exporter:
  enabled: false
  bind-address: "0.0.0.0:9100"

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

"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazy_static::lazy_static;
    use std::fs;
    use std::net::Ipv4Addr;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    lazy_static! {
        static ref CONFIG_TEST_GUARD: Mutex<()> = Mutex::new(());
    }

    fn write_config(file: &PathBuf, contents: &str) {
        fs::write(file, contents).expect("failed to write temp config");
    }

    fn basic_config(target: &str, max_conn: usize) -> String {
        format!(
            r#"bind-address: "127.0.0.1:25565"
proxy_threads: 4
prometheus_exporter:
  enabled: false
  bind-address: "0.0.0.0:9100"
debug: false
redirections:
  - incoming_domain: "example.com"
    target: "{}"
    max_connections_per_second: {}
    max_packet_per_second: 0
    max_ping_response_per_second: 0
"#,
            target, max_conn
        )
    }

    #[test]
    fn retains_previous_mapping_when_resolution_fails() {
        let _guard = CONFIG_TEST_GUARD.lock().unwrap();
        REDIRECTION_MAP.clear();

        let file = NamedTempFile::new().expect("failed to create temp file");
        let path = file.path().to_path_buf();

        write_config(&path, &basic_config("127.0.0.1:25565", 1));
        update_proxies_from_config(path.to_str().unwrap());

        {
            let entry = REDIRECTION_MAP
                .get("example.com")
                .expect("initial mapping missing");
            assert_eq!(entry.ip, Ipv4Addr::new(127, 0, 0, 1));
            assert_eq!(entry.port, 25565);
            assert_eq!(entry.max_connections_per_second, 1);
        }

        write_config(&path, &basic_config("does.not.resolve.invalid:25565", 3));
        update_proxies_from_config(path.to_str().unwrap());

        {
            let entry = REDIRECTION_MAP
                .get("example.com")
                .expect("mapping should persist after resolution failure");
            assert_eq!(entry.ip, Ipv4Addr::new(127, 0, 0, 1));
            assert_eq!(entry.port, 25565);
            assert_eq!(entry.max_connections_per_second, 3);
        }

        REDIRECTION_MAP.clear();
    }

    #[test]
    fn removes_domains_missing_from_config() {
        let _guard = CONFIG_TEST_GUARD.lock().unwrap();
        REDIRECTION_MAP.clear();

        let file = NamedTempFile::new().expect("failed to create temp file");
        let path = file.path().to_path_buf();

        let initial = r#"bind-address: "127.0.0.1:25565"
proxy_threads: 4
prometheus_exporter:
  enabled: false
  bind-address: "0.0.0.0:9100"
debug: false
redirections:
  - incoming_domain: "example.com"
    target: "127.0.0.1:25565"
  - incoming_domain: "other.example"
    target: "127.0.0.1:25566"
"#;
        write_config(&path, initial);
        update_proxies_from_config(path.to_str().unwrap());
        assert!(REDIRECTION_MAP.contains_key("example.com"));
        assert!(REDIRECTION_MAP.contains_key("other.example"));

        let updated = r#"bind-address: "127.0.0.1:25565"
proxy_threads: 4
prometheus_exporter:
  enabled: false
  bind-address: "0.0.0.0:9100"
debug: false
redirections:
  - incoming_domain: "example.com"
    target: "127.0.0.1:25565"
"#;
        write_config(&path, updated);
        update_proxies_from_config(path.to_str().unwrap());

        assert!(REDIRECTION_MAP.contains_key("example.com"));
        assert!(!REDIRECTION_MAP.contains_key("other.example"));

        REDIRECTION_MAP.clear();
    }
}
