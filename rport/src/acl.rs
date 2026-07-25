use ipnet::IpNet;
use std::net::{IpAddr, ToSocketAddrs};

#[derive(Debug, Clone)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    pub fn new(port: u16) -> Self {
        Self { start: port, end: port }
    }

    pub fn range(start: u16, end: u16) -> Self {
        Self { start, end }
    }

    pub fn contains(&self, port: u16) -> bool {
        port >= self.start && port <= self.end
    }
}

#[derive(Debug, Clone)]
pub struct AccessRule {
    pub network: IpNet,
    pub ports: Vec<PortRange>,
}

impl AccessRule {
    pub fn matches(&self, ip: &IpAddr, port: u16) -> bool {
        self.network.contains(ip) && self.ports.iter().any(|r| r.contains(port))
    }
}

#[derive(Debug, Clone)]
pub struct Acl {
    pub rules: Vec<AccessRule>,
}

impl Acl {
    pub fn is_allowed(&self, ip: &IpAddr, port: u16) -> bool {
        if self.rules.is_empty() {
            return true;
        }
        self.rules.iter().any(|r| r.matches(ip, port))
    }
}

/// Parse a single --allow rule.
/// Formats: "port", "port-port", "host:port", "host:port-port".
fn parse_single_rule(rule: &str) -> Result<AccessRule, String> {
    let rule = rule.trim();
    if rule.is_empty() {
        return Err("Empty rule".to_string());
    }

    if let Some((left, right)) = rule.rsplit_once(':') {
        // host:port or host:port-port
        let network: IpNet = if left.contains(':') {
            // IPv6 address — try as /128
            let ip: IpAddr = left.parse().map_err(|e| format!("Invalid IP '{}': {}", left, e))?;
            IpNet::new(ip, 128).map_err(|e| format!("Invalid network: {}", e))?
        } else {
            // Try as a simple IP first, then as network/mask
            if let Ok(ip) = left.parse::<IpAddr>() {
                let prefix = if ip.is_ipv4() { 32 } else { 128 };
                IpNet::new(ip, prefix).map_err(|e| format!("Invalid network: {}", e))?
            } else if let Ok(net) = left.parse::<IpNet>() {
                net
            } else {
                // hostname — resolve to IP
                let ip = resolve_host(left)?;
                let prefix = if ip.is_ipv4() { 32 } else { 128 };
                IpNet::new(ip, prefix).map_err(|e| format!("Invalid network: {}", e))?
            }
        };
        let ports = parse_ports_spec(right)?;
        Ok(AccessRule { network, ports })
    } else {
        // port or port-port (any host)
        let ports = parse_ports_spec(rule)?;
        let network = IpNet::new("0.0.0.0".parse().unwrap(), 0).unwrap();
        Ok(AccessRule { network, ports })
    }
}

fn resolve_host(host: &str) -> Result<IpAddr, String> {
    // Use tokio's blocking resolve in a non-async context
    // Since we're in sync code, use std::net::ToSocketAddrs
    let addrs: Vec<std::net::SocketAddr> = format!("{}:0", host)
        .parse::<std::net::SocketAddr>()
        .ok()
        .map(|a| vec![a])
        .unwrap_or_else(|| {
            (host, 0u16)
                .to_socket_addrs()
                .ok()
                .map(|i| i.collect())
                .unwrap_or_default()
        });
    addrs
        .first()
        .map(|a| a.ip())
        .ok_or_else(|| format!("Cannot resolve hostname: {}", host))
}

fn parse_ports_spec(spec: &str) -> Result<Vec<PortRange>, String> {
    let mut ranges = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((start, end)) = part.split_once('-') {
            let s: u16 = start
                .parse()
                .map_err(|e| format!("Invalid port '{}': {}", start, e))?;
            let e: u16 = end
                .parse()
                .map_err(|e| format!("Invalid port '{}': {}", end, e))?;
            if s > e {
                return Err(format!("Invalid port range '{}': start > end", part));
            }
            ranges.push(PortRange::range(s, e));
        } else {
            let p: u16 = part
                .parse()
                .map_err(|e| format!("Invalid port '{}': {}", part, e))?;
            ranges.push(PortRange::new(p));
        }
    }
    Ok(ranges)
}

/// Parse a list of --allow rules (from CLI) into an Acl.
pub fn parse_allow_rules(rules: &[String]) -> Result<Option<Acl>, String> {
    if rules.is_empty() {
        return Ok(None);
    }
    let mut acl_rules = Vec::new();
    for rule in rules {
        acl_rules.push(parse_single_rule(rule)?);
    }
    Ok(Some(Acl { rules: acl_rules }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    #[test]
    fn test_allow_port() {
        let acl = parse_allow_rules(&["22".to_string()]).unwrap().unwrap();
        assert!(acl.is_allowed(&"127.0.0.1".parse().unwrap(), 22));
        assert!(acl.is_allowed(&"10.0.0.1".parse().unwrap(), 22));
        assert!(!acl.is_allowed(&"10.0.0.1".parse().unwrap(), 80));
    }

    #[test]
    fn test_allow_port_range() {
        let acl = parse_allow_rules(&["3000-4000".to_string()]).unwrap().unwrap();
        assert!(acl.is_allowed(&"127.0.0.1".parse().unwrap(), 3500));
        assert!(acl.is_allowed(&"10.0.0.5".parse().unwrap(), 4000));
        assert!(!acl.is_allowed(&"10.0.0.5".parse().unwrap(), 22));
    }

    #[test]
    fn test_allow_host_port() {
        let acl = parse_allow_rules(&["127.0.0.1:22".to_string()]).unwrap().unwrap();
        assert!(acl.is_allowed(&"127.0.0.1".parse().unwrap(), 22));
        assert!(!acl.is_allowed(&"10.0.0.1".parse().unwrap(), 22));
    }

    #[test]
    fn test_allow_host_port_range() {
        let acl = parse_allow_rules(&["127.0.0.1:3000-4000".to_string()]).unwrap().unwrap();
        assert!(acl.is_allowed(&"127.0.0.1".parse().unwrap(), 3500));
        assert!(!acl.is_allowed(&"127.0.0.1".parse().unwrap(), 22));
        assert!(!acl.is_allowed(&"10.0.0.1".parse().unwrap(), 3500));
    }

    #[test]
    fn test_allow_multi_rules() {
        let acl = parse_allow_rules(&[
            "22".to_string(),
            "3000-4000".to_string(),
            "127.0.0.1:80".to_string(),
        ]).unwrap().unwrap();
        assert!(acl.is_allowed(&"10.0.0.1".parse().unwrap(), 22));
        assert!(acl.is_allowed(&"10.0.0.1".parse().unwrap(), 3500));
        assert!(acl.is_allowed(&"127.0.0.1".parse().unwrap(), 80));
        assert!(!acl.is_allowed(&"10.0.0.1".parse().unwrap(), 80));
    }

    #[test]
    fn test_empty_allow() {
        assert!(parse_allow_rules(&[]).unwrap().is_none());
    }
}
