use std::{
    net::{IpAddr, SocketAddr},
    path::Path,
};

use anyhow::{bail, Context, Result};
use ipnet::IpNet;
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tracing_subscriber::filter::LevelFilter;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub handshake_timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub shutdown_timeout_secs: u64,
    pub max_connections: usize,
    pub auth: Auth,
    pub ip_filter: IpFilter,
    pub logging: Logging,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:1080".parse().unwrap(),
            handshake_timeout_secs: 10,
            connect_timeout_secs: 10,
            shutdown_timeout_secs: 30,
            max_connections: 1024,
            auth: Auth::default(),
            ip_filter: IpFilter::default(),
            logging: Logging::default(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let source = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        // TOML errors can include the source line, which could contain a password.
        let mut config: Self = toml::from_str(&source).map_err(|error| {
            anyhow::anyhow!(
                "invalid TOML config (check field names and value types), byte range {:?}",
                error.span()
            )
        })?;
        config.resolve_password()?;
        config.validate()?;
        Ok(config)
    }

    fn resolve_password(&mut self) -> Result<()> {
        if self.auth.password.is_some() && self.auth.password_env.is_some() {
            bail!("auth.password and auth.password_env cannot both be set");
        }
        if self.auth.enabled {
            if let Some(name) = &self.auth.password_env {
                self.auth.password = Some(std::env::var(name).context(
                    "auth.password_env references a missing or non-Unicode environment variable",
                )?);
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.handshake_timeout_secs == 0 || self.connect_timeout_secs == 0 {
            bail!("handshake_timeout_secs and connect_timeout_secs must be greater than zero");
        }
        if self.max_connections == 0 || self.max_connections > tokio::sync::Semaphore::MAX_PERMITS {
            bail!("max_connections is outside the supported range");
        }
        if self.auth.enabled {
            for (name, value) in [
                ("username", &self.auth.username),
                ("password", &self.auth.password),
            ] {
                if !value.as_ref().is_some_and(|s| (1..=255).contains(&s.len())) {
                    bail!("auth.{name} must contain 1 to 255 UTF-8 bytes when auth is enabled");
                }
            }
        }
        self.logging.level_filter()?;
        Ok(())
    }
}

// Deliberately omit Debug: this type contains credentials.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Auth {
    pub enabled: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    pub password_env: Option<String>,
}

impl Auth {
    pub fn accepts(&self, username: &str, password: &str) -> bool {
        let valid_user = self
            .username
            .as_deref()
            .unwrap_or("")
            .as_bytes()
            .ct_eq(username.as_bytes());
        let valid_password = self
            .password
            .as_deref()
            .unwrap_or("")
            .as_bytes()
            .ct_eq(password.as_bytes());
        bool::from(valid_user & valid_password)
    }
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IpFilter {
    pub enabled: bool,
    pub allow: Vec<IpRule>,
    pub deny: Vec<IpRule>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum IpRule {
    Address(IpAddr),
    Network(IpNet),
}

impl IpRule {
    fn contains(&self, ip: IpAddr) -> bool {
        match self {
            Self::Address(address) => canonical_ip(*address) == ip,
            Self::Network(network) => {
                network.contains(&ip)
                    || match ip {
                        IpAddr::V4(v4) => network.contains(&IpAddr::V6(v4.to_ipv6_mapped())),
                        IpAddr::V6(_) => false,
                    }
            }
        }
    }
}

fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        _ => ip,
    }
}

impl IpFilter {
    pub fn allows(&self, ip: IpAddr) -> bool {
        let ip = canonical_ip(ip);
        !self.enabled
            || (!self.deny.iter().any(|rule| rule.contains(ip))
                && self.allow.iter().any(|rule| rule.contains(ip)))
    }
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub format: LogFormat,
    pub level: String,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            format: LogFormat::Json,
            level: "info".into(),
        }
    }
}

impl Logging {
    pub fn level_filter(&self) -> Result<LevelFilter> {
        self.level
            .parse()
            .context("logging.level must be error, warn, info, debug, or trace")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    Json,
    Text,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(source: &str) -> IpFilter {
        toml::from_str(source).unwrap()
    }

    #[test]
    fn acl_supports_addresses_networks_deny_precedence_and_mapped_ipv4() {
        let acl = filter("enabled = true\nallow = ['10.0.0.0/8', '2001:db8::/32', '::1']\ndeny = ['10.1.0.0/16', '2001:db8::bad']");
        for ip in ["10.2.3.4", "::ffff:10.2.3.4", "2001:db8::1234", "::1"] {
            assert!(acl.allows(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "10.1.2.3",
            "::ffff:10.1.2.3",
            "2001:db8::bad",
            "192.168.1.1",
        ] {
            assert!(!acl.allows(ip.parse().unwrap()), "{ip}");
        }
        let mapped = filter("enabled = true\nallow = ['::ffff:192.0.2.0/120']");
        assert!(mapped.allows("192.0.2.1".parse().unwrap()));
    }

    #[test]
    fn empty_enabled_allow_list_denies_all_but_disabled_filter_allows_all() {
        let ip = "127.0.0.1".parse().unwrap();
        assert!(!filter("enabled = true").allows(ip));
        assert!(filter("enabled = false\ndeny = ['127.0.0.1']").allows(ip));
    }

    #[test]
    fn config_rejects_invalid_rules_and_unknown_fields() {
        assert!(toml::from_str::<Config>("[ip_filter]\nallow = ['10.0.0.0/33']").is_err());
        assert!(toml::from_str::<Config>("[auth]\nenabeld = true").is_err());
        assert!(toml::from_str::<Config>("[ip_filter]\nenabeld = true").is_err());
    }

    #[test]
    fn validates_credentials_and_limits() {
        let mut config = Config::default();
        config.auth.enabled = true;
        assert!(config.validate().is_err());
        config.auth.username = Some("proxy".into());
        config.auth.password = Some("secret".into());
        assert!(config.validate().is_ok());
        assert!(config.auth.accepts("proxy", "secret"));
        assert!(!config.auth.accepts("proxy", "wrong"));
        config.max_connections = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn password_from_environment_and_conflicting_sources() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        // Use a pre-existing variable rather than mutating process-global environment in tests.
        let mut raw: Config =
            toml::from_str("[auth]\nenabled = true\npassword_env = 'PATH'").unwrap();
        raw.resolve_password().unwrap();
        assert_eq!(raw.auth.password.unwrap(), std::env::var("PATH").unwrap());
        std::fs::write(&path, "[auth]\npassword = 'secret'\npassword_env = 'PATH'").unwrap();
        assert!(Config::load(&path).is_err());
        std::fs::write(&path, "[auth]\nenabled = true\nusername = 'proxy'\npassword_env = 'SOCKS_SERVER_TEST_MISSING_6F5A'").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
