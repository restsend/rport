# RPort - WebRTC-based remote port forwarding tool written in Rust

RPort is a modern, WebRTC-based remote port forwarding tool written in Rust. It enables secure peer-to-peer connections for port forwarding, remote access, and network tunneling without requiring complex NAT traversal configurations.

It is built on top of [rustrtc](https://github.com/restsend/rustrtc), a pure Rust WebRTC implementation.

## Features

- 🚀 **WebRTC-based P2P connections** - Direct peer-to-peer tunneling
- 🔒 **Secure tunneling** - End-to-end encrypted connections
- 📁 **Configuration file support** - TOML-based configuration with CLI override
- 🌐 **IPv6 filtering** - Automatic IPv6 candidate filtering for better compatibility
- 🔧 **Multiple operation modes** - Agent, client, and proxy modes
- 🔄 **Background daemon support** - Run as a system daemon with custom log files
- 📊 **Structured logging** - Comprehensive logging with tracing support
- ⚡ **High performance** - Built with Tokio async runtime
- 🛜 **Built-in TURN server** - No need for third-party TURN servers

## Architecture

```
┌─────────────┐    WebRTC P2P    ┌─────────────┐
│   Client    │◄────────────────►│    Agent    │
│             │                  │             │
│ rport -p    │                  │ rport -t    │
│ 8080:22     │                  │ 22 --id     │
└─────────────┘                  └─────────────┘
       │                                │
       │                                │
       ▼                                ▼
┌─────────────┐                ┌─────────────┐
│Local Service│                │Remote Server│
│ :8080       │                │ :22 (SSH)   │
└─────────────┘                └─────────────┘
```

## Quick Start

### 1. Install
```bash
cargo install rport rport-server
```

### 2. Run Server (Coordinator)
```bash
rport-server --addr 0.0.0.0:3000
```

### 3. Run Agent (Remote machine)
```bash
# Forward remote port 22 to the server with ID "my-server"
rport --target 22 --id my-server --token secret-token --server http://your-server:3000
```

### 4. Connect Client (Local machine)
```bash
# Map local port 8080 to remote "my-server"
rport --port 8080 --id my-server --token secret-token --server http://your-server:3000

# Now you can SSH through the tunnel
ssh user@localhost -p 8080
```

## SSH Integration

Easily connect without direct port mapping using `ProxyCommand`:

```bash
# Direct command
ssh -o ProxyCommand='rport --id my-server --token secret-token --server http://your-server:3000' user@localhost

# via ~/.ssh/config
Host my-remote
    ProxyCommand rport --id my-server --token secret-token --server http://your-server:3000
    User ubuntu
```

## Configuration

RPort loads configuration from `~/.rport.toml`:

```toml
token = "secret-token"
server = "http://your-server:3000"

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
```

## Advanced Usage

- **Daemon Mode**: Append `--daemon --log-file rport.log` to the command.
- **Troubleshooting**: Use `RUST_LOG=rport=debug` for verbose logs.
- **NAT**: Built-in TURN server handles most NAT scenarios automatically.

## Security Considerations

- Use strong tokens and restrict configuration file permissions (`chmod 600`).
- Agent listing functionality is disabled to prevent information disclosure.


## Build on linux
```bash
cargo build -r --target x86_64-unknown-linux-musl -p rport
cargo build -r --target x86_64-pc-windows-gnu -p rport
cargo build -r --target aarch64-apple-darwin -p rport
```
