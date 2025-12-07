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

### 1. Install via crates.io
```bash
cargo install rport
cargo install rport-server
```

### 2. Build from source

```bash
## Quick Start

### 1. Build
```bash
git clone https://github.com/restsend/rport.git
cd rport
cargo build -r
```

### 2. Run Server
```bash
# Start coordination server
./target/release/rport-server --addr 0.0.0.0:3000
```

### 3. Run Agent (Remote)
```bash
# Start agent with ID "my-server" forwarding local SSH port 22
./target/release/rport --target 22 --id my-server --token secret-token
```

### 4. Connect Client (Local)
```bash
# Map local port 8080 to remote agent's target
./target/release/rport --port 8080 --id my-server --token secret-token

# Connect via SSH
ssh user@localhost -p 8080
```

## SSH Integration

RPort is designed to work seamlessly with SSH using `ProxyCommand`.

**Direct Usage:**
```bash
ssh -o ProxyCommand='rport --id my-server --token secret-token' user@localhost
```

**SSH Config (`~/.ssh/config`):**
```ssh-config
Host my-remote
    ProxyCommand rport --id my-server --token secret-token
    User ubuntu
```

## Configuration

RPort looks for `~/.rport.toml`.

```toml
token = "secret-token"
# Optional: Custom ICE servers (Built-in TURN is used by default)
# [[ice_servers]]
# urls = ["stun:stun.l.google.com:19302"]
```

## Advanced Usage

**Daemon Mode:**
```bash
./rport --target 22 --id my-server --daemon --log-file /var/log/rport.log
```

**Web/Database Tunneling:**
```bash
# Remote: Share web server (port 80)
./rport --target 80 --id web-server

# Local: Access on port 8080
./rport --port 8080 --id web-server
```

## Troubleshooting

- **NAT Issues**: The built-in TURN server handles most cases. Ensure UDP traffic is allowed.
- **Logs**: Use `RUST_LOG=rport=debug` for verbose output.

## Security Considerations
```

### 2. Start the Server

```bash
# Start the coordination server
./target/release/rport-server --addr 0.0.0.0:3000

# Start with custom TURN server address and public IP
./target/release/rport-server --addr 0.0.0.0:3000 --turn-addr 0.0.0.0:3478 --public-ip 1.2.3.4
```

### 3. Configure and Run Agent

Create a configuration file:

```bash
# Copy example config
cp example-config.toml ~/.rport.toml

# Edit with your settings
nano ~/.rport.toml
```

Start the agent on the remote machine:

```bash
# Run as daemon with custom log file
./target/release/rport --target 22 --id my-server --daemon --log-file /var/log/rport-agent.log

# Or run in foreground for testing
./target/release/rport --target 22 --id my-server --token your-auth-token
```

### 4. Connect from Client

```bash
# Forward local port 8080 to remote SSH (port 22)
./target/release/rport --port 8080 --id my-server --token your-auth-token

# Now you can SSH through the tunnel
ssh user@localhost -p 8080
```

### 5. Use as SSH ProxyCommand

You can use `rport` directly as an SSH ProxyCommand, which avoids opening a local port.

```bash
# Add to ~/.ssh/config
Host my-remote-server
    ProxyCommand rport --id my-server --token your-auth-token -- %h %p
    User your-username
```

## Configuration

### Configuration File

RPort supports configuration files to avoid repeatedly specifying command-line arguments. The configuration file uses TOML format and is loaded in the following order:

1. **Custom path**: Specified with `--conf/-f` parameter
2. **User home**: `~/.rport.toml` (automatically detected)
3. **CLI override**: Command-line arguments override file settings

#### Default Configuration Location

Place your configuration at `~/.rport.toml`:

```toml
# Authentication token (required)
# This token must match between client, agent, and server
token = "your-secure-token-here"

# ICE servers for WebRTC NAT traversal
# Built-in TURN server provides NAT traversal automatically
# Additional STUN servers for redundancy
[[ice_servers]]
urls = ["stun:stun.l.google.com:19302"]

[[ice_servers]]
urls = ["stun:restsend.com:3478"]

# Note: RPort now includes a built-in TURN server
# No additional TURN server configuration is required
# The built-in TURN server automatically handles NAT traversal
```

#### Configuration File Management

```bash
# Create configuration from example
cp example-config.toml ~/.rport.toml

# Edit configuration
nano ~/.rport.toml

# Use custom configuration file
rport --conf /path/to/custom.toml --target 22 --id server1

# Override token from command line (useful for CI/CD)
rport --conf ~/.rport.toml --token $SECRET_TOKEN --target 22 --id server1

# Validate configuration
rport --conf ~/.rport.toml --help
```

#### Configuration Security

```bash
# Set proper permissions for configuration file
chmod 600 ~/.rport.toml

# Store sensitive tokens in environment variables
export RPORT_TOKEN="your-secret-token"
rport --token $RPORT_TOKEN --target 22 --id server1
```

### Command Line Options

#### Agent Mode (Remote Machine)

```bash
rport --target <PORT> --id <AGENT_ID> [OPTIONS]

Options:
  -t, --target <PORT>           Port to forward connections to
  -i, --id <AGENT_ID>          Unique agent identifier
  -s, --server <URL>           Server URL [default: http://127.0.0.1:3000]
  --token <TOKEN>              Authentication token
  -f, --config <FILE>          Configuration file path
  -d, --daemon                 Run as daemon
  --log-file <FILE>            Custom log file for daemon mode
```

#### Client Mode (Local Machine)

```bash
rport --port <LOCAL> --id <AGENT_ID> [OPTIONS]

Options:
  -p, --port <LOCAL>           Port mapping (local)
  -i, --id <AGENT_ID>          Target agent identifier
  -s, --server <URL>           Server URL [default: http://127.0.0.1:3000]
  --token <TOKEN>              Authentication token
  -f, --config <FILE>          Configuration file path
```

#### Server Mode

```bash
rport-server [OPTIONS]

Options:
  -a, --addr <ADDRESS>         Server bind address [default: 127.0.0.1:3000]
```

## Usage Examples

### SSH Tunneling

1. **Setup agent on remote server:**
   ```bash
   # Run SSH agent on remote machine
   ./rport --target 22 --id production-server --daemon
   ```

2. **Connect from local machine:**
   ```bash
   # Forward local port 2222 to remote SSH
   ./rport --port 2222 --id production-server
   
   # SSH through tunnel
   ssh user@localhost -p 2222
   ```

### Web Service Access

1. **Agent on server with web service:**
   ```bash
   ./rport --target 80 --id web-server --daemon
   ```

2. **Access from local browser:**
   ```bash
   ./rport --port 8080 --id web-server
   # Visit http://localhost:8080
   ```

### Database Tunneling

```bash
# Agent on database server
./rport --target 5432 --id db-server --daemon

# Client connection
./rport --port 5432 --id db-server
# Connect to localhost:5432 with your database client
```

## Advanced SSH Integration

### SSH ProxyCommand Usage

RPort can be integrated with SSH using ProxyCommand for seamless remote access without manual port forwarding.

#### Method 1: Direct ProxyCommand

```bash
# Single SSH connection through RPort tunnel
ssh -o ProxyCommand='rport --id production-server --token your-token' user@localhost

# With custom server
ssh -o ProxyCommand='rport --server http://vpn.company.com:3000 --id web-server --token your-token' user@localhost
```

#### Method 2: SSH Config Integration

Create or edit `~/.ssh/config`:

```ssh-config
# RPort tunnel configuration
Host production-server
    HostName localhost
    User your-username
    Port 2222
    ProxyCommand rport --conf ~/.rport.toml --id production-server
    ServerAliveInterval 30
    ServerAliveCountMax 3

# Multiple servers through RPort
Host web-server
    HostName localhost
    User ubuntu
    Port 3333
    ProxyCommand rport --conf ~/.rport.toml --id web-server

Host db-server
    HostName localhost
    User postgres
    Port 5432
    ProxyCommand rport --conf ~/.rport.toml --id database-server
    LocalForward 5432 localhost:5432

# Jump host configuration
Host jump-server
    HostName localhost
    User admin
    Port 2222
    ProxyCommand rport --conf ~/.rport.toml --id jump-host

Host internal-server
    HostName 192.168.1.100
    User developer
    ProxyJump jump-server
```
## Daemon Mode

RPort supports running as a background daemon with comprehensive logging:

```bash
# Start daemon with default log location
./rport --target 22 --id server1 --daemon

# Start daemon with custom log file
./rport --target 22 --id server1 --daemon --log-file /var/log/rport/agent.log

# Check daemon status
ps aux | grep rport

# View logs
tail -f /var/log/rport/agent.log
```

## Troubleshooting

### Common Issues

1. **Connection fails through NAT:**
   - The built-in TURN server should handle most NAT scenarios automatically
   - Check firewall settings for UDP traffic
   - Verify ICE server connectivity
   - Ensure the server is accessible from both client and agent

2. **Token authentication errors:**
   - Ensure token matches between client and agent
   - Check configuration file syntax
   - Verify token in server logs

3. **Port binding errors:**
   - Check if local port is already in use
   - Run with different port numbers
   - Verify permissions for privileged ports (<1024)

4. **Daemon not starting:**
   - Check log file permissions
   - Verify configuration file path
   - Ensure target service is accessible

### Debug Mode

Enable debug logging for detailed troubleshooting:

```bash
RUST_LOG=rport=debug ./rport --target 22 --id debug-agent
```

### Log Locations

- **Daemon mode default logs:** `/tmp/rport-*.log`
- **Custom daemon logs:** Specified by `--log-file`
- **Foreground mode:** Standard output/error

## Security Considerations

- Use strong authentication tokens
- The built-in TURN server provides secure NAT traversal without additional configuration
- Monitor log files for suspicious activity
- Run agents with minimal required privileges
- Use configuration files with appropriate file permissions (600)
- Agent listing functionality has been removed to prevent information disclosure

## Support

For issues and questions:
- Check the troubleshooting section
- Review log files for error details
- Open an issue with reproduction steps
