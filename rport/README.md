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

## Quick Start

### 1. Install
```bash
cargo install rport
```

### 2. Run Agent (Remote machine)
```bash
rport -A 22 -i my-server -k secret-token -s your-server.com:8443
```

### 3. Connect Client (Local machine)
```bash
rport -L 8080:127.0.0.1:22 -i my-server -k secret-token -s your-server.com:8443

# Now you can SSH through the tunnel
ssh user@localhost -p 8080
```

## Configuration

RPort loads configuration from `~/.rport.toml`:

```toml
token = "secret-token"
server = "your-server.com:8443"

[[ice_servers]]
urls = ["stun:stun.l.google.com:19302"]
```

## Advanced Usage

- **Daemon Mode**: `rport -A 22 -i my-server -k token -s server.com:8443 -d --log-file rport.log`
- **Multiple forwards**: `rport -L 8080:127.0.0.1:80 -L 2222:127.0.0.1:22 -i my-server -k token -s server.com:8443`
- **Troubleshooting**: Use `--debug` for verbose logs.

## Security Considerations

- Use strong tokens and restrict configuration file permissions (`chmod 600`).
- Agent listing functionality is disabled to prevent information disclosure.

## Build

```bash
cargo build -r --target x86_64-unknown-linux-musl -p rport
cargo build -r --target x86_64-pc-windows-gnu -p rport
cargo build -r --target aarch64-apple-darwin -p rport
```
