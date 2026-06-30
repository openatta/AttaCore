//! SSRF guard — prevent HTTP hooks from connecting to private/reserved IPs.

/// Errors from SSRF URL checking.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SsrfError {
    #[error("SSRF guard: blocked request to private/reserved IP `{ip}` (url: `{url}`)")]
    PrivateIp { ip: std::net::IpAddr, url: String },
    #[error("SSRF guard: host `{host}` resolves to private IP `{ip}` (url: `{url}`)")]
    HostResolvesToPrivate {
        host: String,
        ip: std::net::IpAddr,
        url: String,
    },
    #[error("SSRF guard: cannot parse url `{0}`")]
    UrlParse(String),
    #[error("SSRF guard: unclosed IPv6 bracket in url `{0}`")]
    UnclosedIpv6(String),
    #[error("SSRF guard: empty host in url `{0}`")]
    EmptyHost(String),
}

/// Check URL against private/reserved IP ranges before connecting.
pub(super) async fn ssrf_check_url(url_str: &str) -> Result<(), SsrfError> {
    let host = parse_host_from_url(url_str)?;
    // If host is an IP literal, check directly (no DNS needed)
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_private_ip(ip) {
            return Err(SsrfError::PrivateIp {
                ip,
                url: url_str.to_string(),
            });
        }
        return Ok(());
    }
    // Resolve hostname to IPs and check each
    let resolved: Vec<std::net::SocketAddr> =
        match tokio::net::lookup_host(format!("{host}:0")).await {
            Ok(addrs) => addrs.collect(),
            Err(e) => {
                // DNS failure is not an SSRF issue — let the request fail naturally
                tracing::debug!(
                    host = %host,
                    error = %e,
                    "ssrf_check: DNS lookup failed, allowing through"
                );
                return Ok(());
            }
        };
    for sa in &resolved {
        if is_private_ip(sa.ip()) {
            return Err(SsrfError::HostResolvesToPrivate {
                host: host.to_string(),
                ip: sa.ip(),
                url: url_str.to_string(),
            });
        }
    }
    Ok(())
}

fn is_private_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_loopback(),
    }
}

/// Extract the host portion from a URL string (e.g. `http://example.com:8080/path` → `example.com`).
fn parse_host_from_url(url_str: &str) -> Result<String, SsrfError> {
    let without_scheme = url_str
        .split_once("://")
        .map(|(_, v)| v)
        .ok_or_else(|| SsrfError::UrlParse(url_str.to_string()))?;
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    // Strip IPv6 brackets if present
    let host = if host_port.starts_with('[') {
        let end = host_port
            .find(']')
            .ok_or_else(|| SsrfError::UnclosedIpv6(url_str.to_string()))?;
        &host_port[1..end]
    } else {
        host_port.split(':').next().unwrap_or(host_port)
    };
    if host.is_empty() {
        return Err(SsrfError::EmptyHost(url_str.to_string()));
    }
    Ok(host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_from_url_basic() {
        assert_eq!(
            parse_host_from_url("http://example.com/path").unwrap(),
            "example.com"
        );
    }

    #[test]
    fn parse_host_from_url_with_port() {
        assert_eq!(
            parse_host_from_url("https://192.168.1.1:8080/route").unwrap(),
            "192.168.1.1"
        );
    }

    #[test]
    fn parse_host_from_url_ipv6() {
        assert_eq!(
            parse_host_from_url("http://[::1]:9090/path").unwrap(),
            "::1"
        );
    }

    #[test]
    fn parse_host_from_url_no_scheme() {
        assert!(parse_host_from_url("no-scheme").is_err());
    }

    #[test]
    fn parse_host_from_url_empty_host() {
        let result = parse_host_from_url("http:///path");
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("empty host"),
            "expected EmptyHost error"
        );
    }

    #[test]
    fn parse_host_from_url_unclosed_ipv6() {
        assert!(parse_host_from_url("http://[::1").is_err());
    }

    #[test]
    fn is_private_ip_v4_loopback() {
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_v4_private() {
        assert!(is_private_ip("10.0.0.5".parse().unwrap()));
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
        assert!(is_private_ip("192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_v4_link_local() {
        assert!(is_private_ip("169.254.1.1".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_v4_public() {
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_v6_loopback() {
        assert!(is_private_ip("::1".parse().unwrap()));
    }

    #[test]
    fn is_private_ip_v6_public() {
        assert!(!is_private_ip("2001:4860:4860::8888".parse().unwrap()));
    }

    #[tokio::test]
    async fn ssrf_check_blocks_loopback() {
        let result = ssrf_check_url("http://127.0.0.1:8080/api").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("SSRF guard"));
    }

    #[tokio::test]
    async fn ssrf_check_blocks_private_ip() {
        let result = ssrf_check_url("http://192.168.1.10/admin").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_check_blocks_ipv6_loopback() {
        let result = ssrf_check_url("http://[::1]:9090/path").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ssrf_check_allows_public_ip() {
        let result = ssrf_check_url("http://8.8.8.8/health").await;
        // DNS resolution of 8.8.8.8 will actually work (it's a public DNS).
        // The IP itself is not private, so this should pass.
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn ssrf_check_allows_public_hostname() {
        let result = ssrf_check_url("http://example.com/api").await;
        // example.com resolves to public IPs, so this should pass.
        assert!(result.is_ok());
    }

    #[test]
    fn ssrf_error_display_private_ip() {
        let err = SsrfError::PrivateIp {
            ip: "127.0.0.1".parse().unwrap(),
            url: "http://127.0.0.1/test".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("SSRF guard"));
        assert!(msg.contains("127.0.0.1"));
    }

    #[test]
    fn ssrf_error_display_url_parse() {
        let err = SsrfError::UrlParse("bad-url".into());
        assert!(err.to_string().contains("bad-url"));
    }
}
