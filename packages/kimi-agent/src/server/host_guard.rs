use std::net::IpAddr;

#[derive(Debug, Clone, Default)]
pub struct HostGuard {
    bind_host: String,

    extra: Vec<String>,
}

impl HostGuard {
    pub fn new(bind_host: impl Into<String>, extra: Vec<String>) -> Self {
        Self {
            bind_host: bind_host.into(),
            extra,
        }
    }

    pub fn allows(&self, host: Option<&str>) -> bool {
        let Some(host) = host.filter(|value| !value.is_empty()) else {
            return false;
        };
        let host = strip_port(host);

        if matches!(host.as_str(), "localhost" | "127.0.0.1" | "::1" | "[::1]") {
            return true;
        }

        if host.ends_with(".localhost") {
            return true;
        }

        let bare = host
            .strip_prefix('[')
            .and_then(|inner| inner.strip_suffix(']'))
            .unwrap_or(&host);
        if bare.parse::<IpAddr>().is_ok() {
            return true;
        }
        if host == strip_port(&self.bind_host) {
            return true;
        }
        self.extra.iter().any(|entry| {
            let entry = strip_port(entry);
            match entry.strip_prefix('.') {
                Some(base) => host == base || host.ends_with(&entry),
                None => host == entry,
            }
        })
    }
}

pub fn split_allowed_hosts(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn strip_port(host: &str) -> String {
    if host.starts_with('[') {
        let end = host.find(']').map_or(host.len(), |index| index + 1);
        return host[..end].to_ascii_lowercase();
    }
    let Some(first) = host.find(':') else {
        return host.to_ascii_lowercase();
    };

    if host.rfind(':') == Some(first) {
        let port = &host[first + 1..];
        if !port.is_empty() && port.bytes().all(|byte| byte.is_ascii_digit()) {
            return host[..first].to_ascii_lowercase();
        }
    }
    host.to_ascii_lowercase()
}

pub fn rejection_message(host: Option<&str>) -> String {
    let normalized = host.filter(|value| !value.is_empty()).map(strip_port);
    let label = normalized
        .clone()
        .unwrap_or_else(|| "<missing>".to_string());
    let argument = normalized.unwrap_or_else(|| "<host>".to_string());
    format!(
        "Invalid Host header: {label}; allow this host with KIMI_CODE_ALLOWED_HOSTS={argument} \
         or 'kimi web --allowed-host {argument}'."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_port_keeps_ipv6_brackets_and_drops_the_port() {
        assert_eq!(strip_port("example.com:8080"), "example.com");
        assert_eq!(strip_port("example.com"), "example.com");
        assert_eq!(strip_port("EXAMPLE.com"), "example.com");
        assert_eq!(strip_port("[::1]:8080"), "[::1]");
        assert_eq!(strip_port("[::1]"), "[::1]");
        assert_eq!(strip_port("::1"), "::1");
        assert_eq!(strip_port("fe80::1"), "fe80::1");
    }

    #[test]
    fn loopback_names_and_ip_literals_are_allowed() {
        let guard = HostGuard::new("127.0.0.1", Vec::new());
        for host in [
            "127.0.0.1",
            "127.0.0.1:58627",
            "localhost",
            "localhost:58627",
            "LOCALHOST",
            "::1",
            "[::1]:58627",
            "app.localhost",
            "192.168.1.5:58627",
            "[fe80::1]:58627",
        ] {
            assert!(guard.allows(Some(host)), "{host} should be allowed");
        }
    }

    #[test]
    fn a_name_that_is_not_the_bind_host_is_refused() {
        let guard = HostGuard::new("127.0.0.1", Vec::new());
        assert!(!guard.allows(Some("evil.example")));
        assert!(!guard.allows(Some("evil.example:58627")));
        assert!(!guard.allows(Some("127.0.0.1.evil.example")));
        assert!(!guard.allows(None));
        assert!(!guard.allows(Some("")));
    }

    #[test]
    fn the_bind_host_is_allowed_even_when_it_is_not_loopback() {
        let guard = HostGuard::new("kimi.internal:58627", Vec::new());
        assert!(guard.allows(Some("kimi.internal")));
        assert!(guard.allows(Some("kimi.internal:58627")));
        assert!(!guard.allows(Some("other.internal")));
    }

    #[test]
    fn extra_hosts_match_exactly_or_as_a_domain_suffix() {
        let guard = HostGuard::new(
            "127.0.0.1",
            vec!["kimi.example".to_string(), ".corp.example".to_string()],
        );
        assert!(guard.allows(Some("kimi.example")));
        assert!(guard.allows(Some("kimi.example:58627")));
        assert!(!guard.allows(Some("sub.kimi.example")));
        assert!(guard.allows(Some("corp.example")));
        assert!(guard.allows(Some("a.b.corp.example")));
        assert!(!guard.allows(Some("notcorp.example")));
    }

    #[test]
    fn split_allowed_hosts_trims_and_drops_empties() {
        assert_eq!(
            split_allowed_hosts("a.example, b.example ,,c.example,"),
            vec!["a.example", "b.example", "c.example"]
        );
        assert!(split_allowed_hosts("").is_empty());
    }

    #[test]
    fn the_rejection_message_names_both_ways_to_allow_the_host() {
        let message = rejection_message(Some("evil.example:58627"));
        assert!(message.contains("evil.example"), "{message}");
        assert!(!message.contains("58627"), "{message}");
        assert!(
            message.contains("KIMI_CODE_ALLOWED_HOSTS=evil.example"),
            "{message}"
        );
        assert!(message.contains("--allowed-host evil.example"), "{message}");

        let missing = rejection_message(None);
        assert!(missing.contains("<missing>"), "{missing}");
        assert!(missing.contains("--allowed-host <host>"), "{missing}");
    }
}
