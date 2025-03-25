# MineShield Proxy 🦀🚀

![Build Status](https://img.shields.io/badge/build-passing-brightgreen)

**MineShield Proxy** is a next-generation high performance Minecraft proxy written in Rust. It’s designed to be fast, efficient, and stable. Capable of handling over 10,000 connections simultaneously. Perfect for server owners who want advanced control over their network traffic, MineShield Proxy comes with built-in target overload prevention, rate limiting, and Proxy Protocol v2 support to ensure your Minecraft server network runs smoothly and securely.

## Features

- **High Performance:**  
  Built with Rust and Rayon for efficient concurrency and low-latency networking.

- **Dynamic Configuration:**  
  Uses a YAML configuration file for easy setup of redirections and advanced features. If no config exists, a default one is automatically generated.

- **Target Overload Prevention:**  
  Prevents a single target server from being overwhelmed by rate limiting connections per second.

- **Configurable Thread Pool:**  
  Customize the number of threads used by the proxy (default is 4) for optimal performance.

- **Proxy Protocol v2:**  
  Ensures that your backend servers receive accurate client IP information.

- **MOTD Cacheing**  
  The proxy pings the target servers in every second and keeps it's cache up to date.


## Flowchart
![FlowChart](https://i.ibb.co/Z6PW1ZNy/Untitled-diagram-2025-03-09-100419.png)
---
## Example ntfy.sh
<img src="https://i.ibb.co/zTBF6Wz9/Screenshot-20250325-190525-ntfy.jpg" width="300"/>
---

## Getting Started

### Bare Metal Installation

1. **Clone the repository:**

   ```bash
   git clone https://github.com/A128K21/MineShield-Proxy.git
   cd mineshieldv2-proxy
   cargo build --release
   ```

2. **Run the proxy:**

   ```bash
   ./target/release/mineshield-proxy
   ```

---
3. **Configure the proxy:**

   Start the proxy and edit the default `config.yml` file in the repository root:

    ```yaml
    # Default configuration for proxy redirections.
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
       
          
    ```



### Docker Installation

1. **Clone the repository and change into the directory:**

   ```bash
   git clone https://github.com/A128K21/MineShield-Proxy.git
   cd mineshieldv2-proxy
   ```

2. **Build the Docker Image:**

   ```bash
   docker build -t mineshield-proxy .
   ```

3. **Run the Docker Container:**

   If you want to use a custom configuration file (`config.yml`), mount it as a volume:

   ```bash
   docker run -d -p 25565:25565 -v /path/to/your/config.yml:/app/config.yml mineshield-proxy
   ```

   Replace `/path/to/your/config.yml` with the actual path to your config file on the host.
---

4. **Happy proxying! 🚀**
