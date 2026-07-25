# RPort - WebRTC-based remote port forwarding tool written in Rust

RPort is a modern, WebRTC-based remote port forwarding tool written in Rust. It enables secure peer-to-peer connections for port forwarding, remote access, and network tunneling without requiring complex NAT traversal configurations.

It is built on top of [rustrtc](https://github.com/restsend/rustrtc), a pure Rust WebRTC implementation.

## Features

- 🚀 **WebRTC-based P2P connections** - Direct peer-to-peer tunneling
- 🔒 **Secure tunneling** - End-to-end encrypted connections over DTLS signaling + WebRTC data channels
- 📁 **Configuration file support** - TOML-based configuration with CLI override
- 🔧 **Multiple operation modes** - Agent (`-A`), client (`-L`), and ProxyCommand modes
- 🔄 **Background daemon support** - Run as a system daemon with `-d` and custom log files
- 📊 **Structured logging** - Comprehensive logging with tracing support
- ⚡ **High performance** - Built with Tokio async runtime
- 🛜 **Built-in TURN server** - No need for third-party TURN servers
- 🔐 **DTLS encryption** - Secure signaling channel with optional certificate authentication

## Quick Start

### 1. Install
```bash
cargo install rport rport-server
```

### 2. Run Server (Coordinator)
The server needs both an HTTP endpoint (for ICE server info and legacy SSE agents) and a DTLS signaling port:
```bash
rport-server --addr 0.0.0.0:3000 --dtls-addr 0.0.0.0:8443
```

### 3. Run Agent (Remote machine)
```bash
# Allow port 22 (SSH) with agent ID "my-server"
rport -A 22 -i my-server -k secret-token -s your-server.com:8443
```

### 4. Connect Client (Local machine)
```bash
# Forward local port 8080 to remote agent's port 22 (SSH)
rport -L 8080:127.0.0.1:22 -i my-server -k secret-token -s your-server.com:8443

# Now you can SSH through the tunnel
ssh user@localhost -p 8080
```

## SSH Integration

Easily connect without direct port mapping using `ProxyCommand`:

```bash
# Direct command
ssh -o ProxyCommand='rport -L 127.0.0.1:22 -i my-server -k secret-token -s your-server.com:8443' user@localhost

# via ~/.ssh/config
Host my-remote
    ProxyCommand rport -L 127.0.0.1:22 -i my-server -k secret-token -s your-server.com:8443
    User ubuntu
```

## Configuration

RPort loads configuration from `~/.rport.toml`:

```toml
token = "secret-token"
server = "your-server.com:8443"

# Optional: Add ICE servers
[[ice_servers]]
urls = ["stun:stun.l.google.com:19302"]
```

### Advanced WebRTC Settings

These can be added to `~/.rport.toml` for tuning:

```toml
# ICE disconnect detection: time without traffic before ICE goes Disconnected (s)
ice_disconnect_threshold = 30

# ICE disconnect grace: how long to wait in Disconnected before tearing down (s)
ice_disconnect_grace = 15

# ICE hard timeout: total time without traffic before ICE goes Failed (s)
ice_connection_timeout = 300

# SCTP retransmission parameters
sctp_rto_initial_ms = 400     # Initial RTO (ms)
sctp_rto_min_ms = 200          # Minimum RTO (ms)
sctp_rto_max_sec = 30          # Maximum RTO (s)
sctp_max_association_retransmits = 20
sctp_receive_window_kb = 2048  # Receiver window (KB)

# UPnP port mapping for NAT traversal
enable_upnp = true

# Log file (also settable via --log-file CLI)
log_file = "/var/log/rport.log"
```

## Advanced Usage

- **Daemon Mode**: `rport -A 22 -i my-server -k token -s server.com:8443 -d --log-file rport.log`
- **Agent ACL (multiple ports)**: `rport -A 22 -A 3000-4000 -A 127.0.0.1:8080 -i my-agent -k token -s server.com:8443`
- **Multiple forwards**: `rport -L 8080:127.0.0.1:80 -L 2222:127.0.0.1:22 -i my-server -k token -s server.com:8443`
- **Troubleshooting**: Use `--debug` for verbose logs.
- **NAT**: Built-in TURN server handles most NAT scenarios automatically.

## CLI Reference

### rport (client/agent)
```
Usage: rport [OPTIONS]

Options:
  -f, --conf <CONFIG>        Configuration file path
  -s, --server <SERVER>      DTLS signaling server address (e.g. rport.example.com:8443)
  -k, --token <TOKEN>        Authentication token
  -i, --id <ID>              Agent/client identifier
  -L <SPEC>                  Forward spec: local_port:host:port or host:port (ProxyCommand)
  -A, --allow <RULE>         Access control rule (repeatable): port, port-port, or host:port-port
      --timeout <TIMEOUT>    Connection timeout in seconds
      --debug                Enable debug logging
      --upnp                 Enable UPnP
  -d, --daemon               Daemonize the process (Unix only)
      --log-file <LOG_FILE>  Log file path
  -h, --help                 Print help
  -V, --version              Print version
```

### rport-server
```
Usage: rport-server [OPTIONS]

Options:
  -a, --addr <ADDR>            HTTP server bind address [default: 0.0.0.0:3000]
      --dtls-addr <DTLS_ADDR>  DTLS signaling server bind address
      --dtls-cert <DTLS_CERT>  DTLS certificate path
      --dtls-key <DTLS_KEY>    DTLS private key path
      --disable-turn           Disable TURN server
  -t, --turn-addr <TURN_ADDR>  TURN server address [default: ip:13478]
      --public-ip <PUBLIC_IP>  Public IP address for TURN
      --debug                  Enable debug logging
  -h, --help                   Print help
  -V, --version                Print version
```

## Security Considerations

- Use strong tokens and restrict configuration file permissions (`chmod 600`).
- Agent listing functionality is disabled to prevent information disclosure.
- For production DTLS, provide certificates via `--dtls-cert` and `--dtls-key` instead of using self-signed certificates.

## Build

```bash
cargo build -r --target x86_64-unknown-linux-musl -p rport
cargo build -r --target x86_64-pc-windows-gnu -p rport
cargo build -r --target aarch64-apple-darwin -p rport
```
