use ipnet::IpNet;
use std::net::IpAddr;

#[derive(Debug, Clone)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl PortRange {
    pub fn new(port: u16) -> Self {
        Self {
            start: port,
            end: port,
        }
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
        self.rules.iter().any(|r| r.matches(ip, port))
    }
}

pub fn parse_acl_spec(spec: &str) -> Result<Acl, String> {
    let mut rules = Vec::new();
    for segment in spec.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        let (network_part, ports_part) = segment.split_once(':').ok_or_else(|| {
            format!("Invalid rule '{}': expected network:ports format", segment)
        })?;
        let network: IpNet = network_part
            .parse()
            .map_err(|e| format!("Invalid network '{}': {}", network_part, e))?;
        let ports = parse_ports_spec(ports_part)?;
        rules.push(AccessRule { network, ports });
    }
    Ok(Acl { rules })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_acl_simple() {
        let acl = parse_acl_spec("127.0.0.0/8:22,80,443").unwrap();
        assert!(acl.is_allowed(&"127.0.0.1".parse().unwrap(), 22));
        assert!(acl.is_allowed(&"127.0.0.1".parse().unwrap(), 80));
        assert!(!acl.is_allowed(&"127.0.0.1".parse().unwrap(), 8080));
        assert!(!acl.is_allowed(&"10.0.0.1".parse().unwrap(), 22));
    }

    #[test]
    fn test_acl_multi_range() {
        let acl = parse_acl_spec("10.0.0.0/8:3000-4000,5432").unwrap();
        assert!(acl.is_allowed(&"10.0.0.5".parse().unwrap(), 3500));
        assert!(acl.is_allowed(&"10.0.0.5".parse().unwrap(), 5432));
        assert!(!acl.is_allowed(&"10.0.0.5".parse().unwrap(), 6379));
        assert!(!acl.is_allowed(&"192.168.1.1".parse().unwrap(), 3500));
    }

    #[test]
    fn test_acl_multi_rule() {
        let acl = parse_acl_spec("127.0.0.0/8:22;10.0.0.0/8:3000-4000").unwrap();
        assert!(acl.is_allowed(&"127.0.0.1".parse().unwrap(), 22));
        assert!(acl.is_allowed(&"10.0.0.5".parse().unwrap(), 3500));
        assert!(!acl.is_allowed(&"192.168.1.1".parse().unwrap(), 22));
    }
}
