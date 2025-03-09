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
  
- **IP & Domain Filtering:**  
  Basic filtering is built-in, with room for customization to suit your security needs.


## Flowchart
![FlowChart](https://i.ibb.co/Z6PW1ZNy/Untitled-diagram-2025-03-09-100419.png)


## Getting Started

### Bare Metal Installation

1. **Clone the repository:**

   ```bash
   git clone https://github.com/A128K21/MineShield-Proxy.git
   cd mineshieldv2-proxy
   cargo build --release
   ```

2. **Configure the proxy:**

   Create or edit the `config.yml` file in the repository root with the following content:

   ```yaml
   # Default configuration for proxy redirections.
   prevent_target_overload:
     # Set `enabled` to true to enable target overload prevention.
     enabled: false
     # The `rate_limit_per_target` field specifies the maximum number of requests allowed per second.
     rate_limit_per_target: 10

   # Number of threads to use for the proxy (only used at startup)
   proxy_threads: 4

   redirections:
     - incoming_domain: "example.com"
       target: "192.168.1.100:8080"
     - incoming_domain: "test.com"
       target: "some.domain.com:9090"
   ```

3. **Run the proxy:**

   ```bash
   ./target/release/mineshield-proxy
   ```

---

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
    Feel free to adjust sections, links, and badges as needed to match your project details. This README should give users and contributors a clear understanding of what your proxy does, how to set it up, and how to contribute.
