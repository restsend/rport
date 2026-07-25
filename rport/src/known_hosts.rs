#![allow(dead_code)]
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct KnownHostEntry {
    pub addr: String,
    pub fingerprint: String,
    pub accepted_at: String,
}

#[derive(Debug, Clone)]
pub struct KnownHosts {
    path: PathBuf,
    entries: Vec<KnownHostEntry>,
}

impl KnownHosts {
    pub fn default_path() -> PathBuf {
        home::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".rport-known-hosts")
    }

    pub fn load() -> Self {
        let path = Self::default_path();
        let entries = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .map(|content| {
                    content
                        .lines()
                        .filter_map(|line| {
                            let line = line.trim();
                            if line.is_empty() || line.starts_with('#') {
                                return None;
                            }
                            let parts: Vec<&str> = line.splitn(3, ' ').collect();
                            if parts.len() >= 2 {
                                Some(KnownHostEntry {
                                    addr: parts[0].to_string(),
                                    fingerprint: parts[1].to_string(),
                                    accepted_at: parts.get(2).unwrap_or(&"").to_string(),
                                })
                            } else {
                                None
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        Self { path, entries }
    }

    pub fn save(&self) -> std::io::Result<()> {
        let content = self
            .entries
            .iter()
            .map(|e| format!("{} {} {}", e.addr, e.fingerprint, e.accepted_at))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&self.path, content + "\n")
    }

    pub fn check(&self, addr: &str, fingerprint: &str) -> CheckResult {
        for entry in &self.entries {
            if entry.addr == addr {
                if entry.fingerprint == fingerprint {
                    return CheckResult::Match;
                } else {
                    return CheckResult::Mismatch {
                        expected: entry.fingerprint.clone(),
                        actual: fingerprint.to_string(),
                    };
                }
            }
        }
        CheckResult::Unknown
    }

    pub fn add(&mut self, addr: &str, fingerprint: &str) {
        let entry = KnownHostEntry {
            addr: addr.to_string(),
            fingerprint: fingerprint.to_string(),
            accepted_at: chrono::Utc::now()
                .format("(accepted on %Y-%m-%d %H:%M:%S UTC)")
                .to_string(),
        };
        self.entries.retain(|e| e.addr != addr);
        self.entries.push(entry);
    }

    pub fn prompt_and_confirm(addr: &str, fingerprint: &str) -> bool {
        eprintln!();
        eprintln!("The authenticity of host '[{}]' can't be established.", addr);
        eprintln!("Fingerprint: {}", fingerprint);
        eprintln!("Are you sure you want to continue connecting? (yes/no)");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).ok();
        let input = input.trim().to_lowercase();
        input == "yes" || input == "y"
    }
}

pub enum CheckResult {
    Match,
    Mismatch { expected: String, actual: String },
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_known_hosts_check() {
        let kh = KnownHosts {
            path: PathBuf::from("/tmp/test_kh"),
            entries: vec![KnownHostEntry {
                addr: "192.168.1.5:4443".to_string(),
                fingerprint: "AA:BB:CC:DD".to_string(),
                accepted_at: String::new(),
            }],
        };

        match kh.check("192.168.1.5:4443", "AA:BB:CC:DD") {
            CheckResult::Match => {}
            _ => panic!("Expected Match"),
        }

        match kh.check("192.168.1.5:4443", "XX:YY:ZZ") {
            CheckResult::Mismatch { .. } => {}
            _ => panic!("Expected Mismatch"),
        }

        match kh.check("10.0.0.1:4443", "AA:BB:CC:DD") {
            CheckResult::Unknown => {}
            _ => panic!("Expected Unknown"),
        }
    }

    #[test]
    fn test_add_and_check() {
        let mut kh = KnownHosts {
            path: PathBuf::from("/tmp/test_kh2"),
            entries: vec![],
        };

        assert!(matches!(kh.check("host:4443", "FP:123"), CheckResult::Unknown));

        kh.add("host:4443", "FP:123");
        assert!(matches!(kh.check("host:4443", "FP:123"), CheckResult::Match));

        // Updating should work
        kh.add("host:4443", "FP:456");
        assert!(matches!(kh.check("host:4443", "FP:456"), CheckResult::Match));
    }
}
