use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http,
};
use http::{request::Parts, StatusCode};
use std::{
    fmt::{self, Formatter},
    net::{IpAddr, Ipv4Addr, SocketAddr},
};

pub struct ClientAddr {
    pub addr: SocketAddr,
    pub is_secure: bool,
}

impl ClientAddr {
    pub fn new(addr: SocketAddr) -> Self {
        ClientAddr {
            addr,
            is_secure: false,
        }
    }
    pub fn ip(&self) -> IpAddr {
        self.addr.ip()
    }
}

impl<S> FromRequestParts<S> for ClientAddr
where
    S: Send + Sync,
{
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let is_secure = match parts.uri.scheme_str() {
            Some("wss") | Some("https") => true,
            _ => parts
                .headers
                .get("x-forwarded-proto")
                .map_or(false, |v| v == "https"),
        };
        let mut remote_addr = match parts.extensions.get::<ConnectInfo<SocketAddr>>() {
            Some(ConnectInfo(addr)) => addr.clone(),
            None => {
                return Ok(ClientAddr {
                    addr: SocketAddr::from((IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 0)),
                    is_secure,
                });
            }
        };

        for header in [
            "x-client-ip",
            "x-forwarded-for",
            "x-real-ip",
            "cf-connecting-ip",
        ] {
            if let Some(value) = parts.headers.get(header) {
                if let Ok(ip) = value.to_str() {
                    // Handle comma-separated IPs (e.g. X-Forwarded-For can have multiple)
                    let first_ip = ip.split(',').next().unwrap_or(ip).trim();
                    remote_addr.set_ip(IpAddr::V4(first_ip.parse().unwrap()));
                    break;
                }
            }
        }
        Ok(ClientAddr {
            addr: remote_addr,
            is_secure,
        })
    }
}

impl fmt::Display for ClientAddr {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.addr)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn extract(req: http::Request<()>) -> ClientAddr {
        let (mut parts, _) = req.into_parts();
        ClientAddr::from_request_parts(&mut parts, &())
            .await
            .expect("extract ClientAddr")
    }

    #[tokio::test]
    async fn test_plain_http_not_secure() {
        let addr = extract(
            http::Request::builder()
                .uri("http://example.com/rport/connect")
                .body(())
                .unwrap(),
        )
        .await;
        assert!(!addr.is_secure);
        // No ConnectInfo extension -> fallback 0.0.0.0:0
        assert_eq!(addr.to_string(), "0.0.0.0:0");
    }

    #[tokio::test]
    async fn test_wss_scheme_is_secure() {
        let addr = extract(
            http::Request::builder()
                .uri("wss://example.com/rport/connect")
                .body(())
                .unwrap(),
        )
        .await;
        assert!(addr.is_secure);
    }

    #[tokio::test]
    async fn test_https_scheme_is_secure() {
        let addr = extract(
            http::Request::builder()
                .uri("https://example.com/rport/connect")
                .body(())
                .unwrap(),
        )
        .await;
        assert!(addr.is_secure);
    }

    #[tokio::test]
    async fn test_forwarded_proto_header_marks_secure() {
        let addr = extract(
            http::Request::builder()
                .uri("http://example.com/")
                .header("x-forwarded-proto", "https")
                .body(())
                .unwrap(),
        )
        .await;
        assert!(addr.is_secure);
    }

    #[tokio::test]
    async fn test_x_forwarded_for_first_ip_wins() {
        let addr = extract(
            http::Request::builder()
                .uri("http://example.com/")
                .header("x-forwarded-for", "203.0.113.7, 10.0.0.1")
                .extension(ConnectInfo(
                    "192.168.1.5:5000".parse::<SocketAddr>().unwrap(),
                ))
                .body(())
                .unwrap(),
        )
        .await;
        assert_eq!(addr.ip().to_string(), "203.0.113.7");
        // Port from ConnectInfo is preserved
        assert_eq!(addr.addr.port(), 5000);
    }

    #[tokio::test]
    async fn test_x_client_ip_has_priority() {
        let addr = extract(
            http::Request::builder()
                .uri("http://example.com/")
                .header("x-client-ip", "198.51.100.9")
                .header("x-forwarded-for", "203.0.113.7")
                .header("x-real-ip", "192.0.2.1")
                .extension(ConnectInfo(
                    "192.168.1.5:5000".parse::<SocketAddr>().unwrap(),
                ))
                .body(())
                .unwrap(),
        )
        .await;
        assert_eq!(addr.ip().to_string(), "198.51.100.9");
    }

    #[tokio::test]
    async fn test_connect_info_fallback_without_headers() {
        let addr = extract(
            http::Request::builder()
                .uri("http://example.com/")
                .extension(ConnectInfo(
                    "192.168.1.5:5000".parse::<SocketAddr>().unwrap(),
                ))
                .body(())
                .unwrap(),
        )
        .await;
        assert_eq!(addr.to_string(), "192.168.1.5:5000");
    }

    #[test]
    fn test_new_defaults_to_insecure() {
        let addr = ClientAddr::new("10.1.2.3:8080".parse().unwrap());
        assert!(!addr.is_secure);
        assert_eq!(addr.ip().to_string(), "10.1.2.3");
        assert_eq!(addr.to_string(), "10.1.2.3:8080");
    }
}
