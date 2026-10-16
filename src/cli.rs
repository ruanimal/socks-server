use std::{
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::Result;
use clap::{ArgAction, Parser};
use tracing_subscriber::filter::LevelFilter;

use crate::config::{Config, IpRule, LogFormat};

// Do not derive Debug: command-line arguments can contain a password.
#[derive(Parser)]
#[command(
    version,
    about,
    after_help = "Precedence: command line > config file > defaults. Without --config, config.toml is used if present. IP lists supplied on the command line replace the corresponding config list."
)]
pub struct Args {
    /// TOML configuration file (default: config.toml if present)
    #[arg(short, long)]
    pub config: Option<PathBuf>,
    /// Validate the merged configuration and exit
    #[arg(long)]
    pub check: bool,
    /// Listener address, e.g. 127.0.0.1:1080 or [::]:1080
    #[arg(short = 'l', long)]
    listen: Option<SocketAddr>,
    /// Authentication and command request timeout in seconds
    #[arg(long)]
    handshake_timeout_secs: Option<u64>,
    /// DNS resolution and destination connection timeout in seconds
    #[arg(long)]
    connect_timeout_secs: Option<u64>,
    /// Shutdown grace period in seconds (0 closes immediately)
    #[arg(long)]
    shutdown_timeout_secs: Option<u64>,
    /// Maximum concurrent client connections
    #[arg(long)]
    max_connections: Option<usize>,
    /// Enable or disable username/password authentication (bare flag means true)
    #[arg(long, action = ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    auth_enabled: Option<bool>,
    /// SOCKS5 username (does not enable authentication by itself)
    #[arg(short = 'u', long)]
    username: Option<String>,
    /// SOCKS5 password; overrides the configured password or password_env
    #[arg(short = 'p', long, conflicts_with = "password_env")]
    password: Option<String>,
    /// Environment variable containing the password; overrides the configured password source
    #[arg(long, conflicts_with = "password")]
    password_env: Option<String>,
    /// Enable or disable client IP filtering (bare flag means true)
    #[arg(long, action = ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
    ip_filter_enabled: Option<bool>,
    /// Allowed client IPs/CIDRs; repeat or comma-separate values; replaces the config allow list
    #[arg(long, value_name = "IP_OR_CIDR", value_delimiter = ',', num_args = 1..)]
    allow: Option<Vec<IpRule>>,
    /// Denied client IPs/CIDRs; repeat or comma-separate values; replaces the config deny list
    #[arg(long, value_name = "IP_OR_CIDR", value_delimiter = ',', num_args = 1..)]
    deny: Option<Vec<IpRule>>,
    /// Replace the configured allow list with an empty list
    #[arg(long, conflicts_with = "allow")]
    clear_allow: bool,
    /// Replace the configured deny list with an empty list
    #[arg(long, conflicts_with = "deny")]
    clear_deny: bool,
    /// Connection log format (default: text, unless set in the config file)
    #[arg(long, value_enum)]
    log_format: Option<LogFormat>,
    /// Connection log level: error, warn, info, debug, trace, or off
    #[arg(long)]
    log_level: Option<LevelFilter>,
}

impl Args {
    pub fn load_config(&self) -> Result<Config> {
        self.load_with_default(Path::new("config.toml"))
    }

    fn load_with_default(&self, default_path: &Path) -> Result<Config> {
        let path = self.config.as_deref().unwrap_or(default_path);
        let mut config = match Config::read(path) {
            Ok(config) => config,
            Err(error)
                if self.config.is_none()
                    && error
                        .downcast_ref::<io::Error>()
                        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound) =>
            {
                Config::default()
            }
            Err(error) => return Err(error),
        };
        self.apply(&mut config);
        // Resolve environment secrets and validate only after command-line overrides are applied.
        config.finish()
    }

    fn apply(&self, config: &mut Config) {
        if let Some(value) = self.listen {
            config.listen = value;
        }
        if let Some(value) = self.handshake_timeout_secs {
            config.handshake_timeout_secs = value;
        }
        if let Some(value) = self.connect_timeout_secs {
            config.connect_timeout_secs = value;
        }
        if let Some(value) = self.shutdown_timeout_secs {
            config.shutdown_timeout_secs = value;
        }
        if let Some(value) = self.max_connections {
            config.max_connections = value;
        }
        if let Some(value) = self.auth_enabled {
            config.auth.enabled = value;
        }
        if let Some(value) = &self.username {
            config.auth.username = Some(value.clone());
        }
        if let Some(value) = &self.password {
            config.auth.password = Some(value.clone());
            config.auth.password_env = None;
        }
        if let Some(value) = &self.password_env {
            config.auth.password_env = Some(value.clone());
            config.auth.password = None;
        }
        if let Some(value) = self.ip_filter_enabled {
            config.ip_filter.enabled = value;
        }
        if let Some(value) = &self.allow {
            config.ip_filter.allow = value.clone();
        }
        if let Some(value) = &self.deny {
            config.ip_filter.deny = value.clone();
        }
        if self.clear_allow {
            config.ip_filter.allow.clear();
        }
        if self.clear_deny {
            config.ip_filter.deny.clear();
        }
        if let Some(value) = self.log_format {
            config.logging.format = value;
        }
        if let Some(value) = self.log_level {
            config.logging.level = value.to_string().to_ascii_lowercase();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(options: &[&str]) -> Args {
        Args::try_parse_from(std::iter::once("socks-server").chain(options.iter().copied()))
            .unwrap()
    }

    #[test]
    fn omitted_cli_values_preserve_config_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let config = args(&[]).load_with_default(&path).unwrap();
        assert_eq!(config.listen, Config::default().listen);
        assert!(matches!(config.logging.format, LogFormat::Text));
        std::fs::write(&path, "listen = '127.0.0.1:9000'\n[auth]\nenabled = true\nusername = 'file-user'\npassword = 'file-password'\n[ip_filter]\nenabled = true\nallow = ['::1']").unwrap();
        let config = args(&[]).load_with_default(&path).unwrap();
        assert_eq!(config.listen.port(), 9000);
        assert!(config.auth.enabled);
        assert!(config.auth.accepts("file-user", "file-password"));
        assert!(config.ip_filter.enabled);
        assert!(config.ip_filter.allows("::1".parse().unwrap()));
        assert!(!config.ip_filter.allows("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn cli_overrides_every_scalar_before_validation_and_secret_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "handshake_timeout_secs = 0\nconnect_timeout_secs = 0\nmax_connections = 0\n[auth]\nenabled = true\nusername = ''\npassword_env = 'SOCKS_SERVER_TEST_MISSING_6F5A'\n[logging]\nlevel = 'invalid'").unwrap();
        let config = args(&[
            "-l",
            "[::1]:9001",
            "--handshake-timeout-secs",
            "5",
            "--connect-timeout-secs",
            "6",
            "--shutdown-timeout-secs",
            "0",
            "--max-connections",
            "20",
            "-u",
            "cli-user",
            "-p",
            "cli-password",
            "--log-format",
            "json",
            "--log-level",
            "debug",
        ])
        .load_with_default(&path)
        .unwrap();
        assert_eq!(config.listen, "[::1]:9001".parse::<SocketAddr>().unwrap());
        assert_eq!(config.handshake_timeout_secs, 5);
        assert_eq!(config.connect_timeout_secs, 6);
        assert_eq!(config.shutdown_timeout_secs, 0);
        assert_eq!(config.max_connections, 20);
        assert!(config.auth.accepts("cli-user", "cli-password"));
        assert!(config.auth.password_env.is_none());
        assert!(matches!(config.logging.format, LogFormat::Json));
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn cli_boolean_values_can_enable_and_disable_independently() {
        let mut config = Config::default();
        args(&["--auth-enabled", "--ip-filter-enabled=false"]).apply(&mut config);
        assert!(config.auth.enabled);
        assert!(!config.ip_filter.enabled);
        args(&["--auth-enabled", "false", "--ip-filter-enabled", "true"]).apply(&mut config);
        assert!(!config.auth.enabled);
        assert!(config.ip_filter.enabled);
    }

    #[test]
    fn cli_lists_replace_file_lists_and_can_be_cleared() {
        let mut config: Config = toml::from_str(
            "[ip_filter]\nenabled = true\nallow = ['192.0.2.1']\ndeny = ['127.0.0.1']",
        )
        .unwrap();
        args(&[
            "--allow",
            "127.0.0.0/8,::1",
            "--allow",
            "2001:db8::/32",
            "--deny",
            "127.0.0.2",
            "127.0.0.3",
        ])
        .apply(&mut config);
        assert_eq!(config.ip_filter.allow.len(), 3);
        assert_eq!(config.ip_filter.deny.len(), 2);
        assert!(config.ip_filter.allows("127.0.0.1".parse().unwrap()));
        assert!(config.ip_filter.allows("::1".parse().unwrap()));
        assert!(!config.ip_filter.allows("127.0.0.2".parse().unwrap()));
        assert!(!config.ip_filter.allows("192.0.2.1".parse().unwrap()));
        args(&["--clear-allow", "--clear-deny"]).apply(&mut config);
        assert!(config.ip_filter.allow.is_empty());
        assert!(config.ip_filter.deny.is_empty());
        assert!(!config.ip_filter.allows("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn password_env_replaces_inline_password_and_auth_can_disable_missing_env() {
        let mut config: Config = toml::from_str("[auth]\npassword = 'from-file'").unwrap();
        args(&["--password-env", "PATH"]).apply(&mut config);
        assert!(config.auth.password.is_none());
        assert_eq!(config.auth.password_env.as_deref(), Some("PATH"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[auth]\nenabled = true\npassword_env = 'SOCKS_SERVER_TEST_MISSING_6F5A'",
        )
        .unwrap();
        let config = args(&["--auth-enabled=false"])
            .load_with_default(&path)
            .unwrap();
        assert!(!config.auth.enabled);
    }

    #[test]
    fn explicit_missing_config_and_malformed_default_config_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert!(args(&["--config", path.to_str().unwrap()])
            .load_config()
            .is_err());
        std::fs::write(&path, "not a TOML file").unwrap();
        assert!(args(&[]).load_with_default(&path).is_err());
    }

    #[test]
    fn rejects_conflicting_and_invalid_cli_options() {
        for options in [
            vec!["--password", "secret", "--password-env", "PATH"],
            vec!["--allow", "127.0.0.1", "--clear-allow"],
            vec!["--deny", "::1", "--clear-deny"],
            vec!["--allow", "192.0.2.0/33"],
            vec!["--auth-enabled=maybe"],
            vec!["--log-format", "yaml"],
        ] {
            assert!(Args::try_parse_from(std::iter::once("socks-server").chain(options)).is_err());
        }
    }
}
