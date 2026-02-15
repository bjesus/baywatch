use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DEFAULT_IDLE_TIMEOUT: u64 = 600; // 10 minutes
const DEFAULT_SHUTDOWN_GRACE: u64 = 5;
const DEFAULT_DOMAIN: &str = "localhost";
const DEFAULT_BIND: &str = "0.0.0.0";
const DEFAULT_PORT: u16 = 80;
const DEFAULT_STARTUP_TIMEOUT: u64 = 30;

/// Raw config as deserialized from YAML.
#[derive(Deserialize)]
struct RawConfig {
    bind: Option<String>,
    port: Option<u16>,
    domain: Option<String>,
    shutdown_grace: Option<u64>,
    startup_timeout: Option<u64>,
    idle_timeout: Option<u64>,
    services: HashMap<String, RawServiceConfig>,
}

#[derive(Deserialize)]
struct RawServiceConfig {
    port: u16,
    command: String,
    pwd: Option<String>,
    idle_timeout: Option<u64>,
    #[serde(default)]
    env: HashMap<String, String>,
}

/// Fully resolved config with defaults applied.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: String,
    pub port: u16,
    pub domain: String,
    pub shutdown_grace: u64,
    pub startup_timeout: u64,
    pub services: HashMap<String, ServiceConfig>,
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub port: u16,
    pub command: String,
    pub pwd: PathBuf,
    pub env: HashMap<String, String>,
    pub idle_timeout: u64,
}

impl Config {
    /// Load config from a file path.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let contents = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {}", path.display(), e))?;
        Self::parse(&contents)
    }

    /// Parse config from a YAML string.
    fn parse(yaml: &str) -> anyhow::Result<Self> {
        let raw: RawConfig = serde_yaml::from_str(yaml)?;

        let global_idle_timeout = raw.idle_timeout.unwrap_or(DEFAULT_IDLE_TIMEOUT);

        let mut services = HashMap::new();
        for (name, raw_svc) in raw.services {
            let pwd = match &raw_svc.pwd {
                Some(p) => {
                    let expanded = shellexpand::tilde(p).to_string();
                    PathBuf::from(expanded)
                }
                None => std::env::current_dir()?,
            };

            services.insert(
                name.clone(),
                ServiceConfig {
                    port: raw_svc.port,
                    command: raw_svc.command,
                    pwd,
                    idle_timeout: raw_svc.idle_timeout.unwrap_or(global_idle_timeout),
                    env: raw_svc.env,
                },
            );
        }

        Ok(Config {
            bind: raw.bind.unwrap_or_else(|| DEFAULT_BIND.to_string()),
            port: raw.port.unwrap_or(DEFAULT_PORT),
            domain: raw.domain.unwrap_or_else(|| DEFAULT_DOMAIN.to_string()),
            shutdown_grace: raw.shutdown_grace.unwrap_or(DEFAULT_SHUTDOWN_GRACE),
            startup_timeout: raw.startup_timeout.unwrap_or(DEFAULT_STARTUP_TIMEOUT),
            services,
        })
    }
}

/// Resolve the config file path.
/// Priority:
///   1. --config flag (explicit)
///   2. $XDG_CONFIG_HOME/baywatch/config.yaml (if it exists)
///   3. ~/.config/baywatch/config.yaml (if it exists)
///   4. /etc/baywatch/config.yaml (system-wide fallback)
pub fn resolve_config_path(cli_path: Option<&str>) -> PathBuf {
    if let Some(p) = cli_path {
        return PathBuf::from(shellexpand::tilde(p).to_string());
    }

    // Try XDG user config
    if let Some(config_dir) = dirs::config_dir() {
        let user_config = config_dir.join("baywatch").join("config.yaml");
        if user_config.exists() {
            return user_config;
        }
    }

    // Fallback to /etc
    let system_config = PathBuf::from("/etc/baywatch/config.yaml");
    if system_config.exists() {
        return system_config;
    }

    // Nothing found — return the XDG path so the error message is useful
    dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("baywatch")
        .join("config.yaml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_config() {
        let yaml = r#"
bind: 127.0.0.1
port: 8080
domain: example.com
shutdown_grace: 10
idle_timeout: 300
services:
  web:
    port: 3000
    command: npm start
    pwd: ~/projects/web
    idle_timeout: 120
  api:
    port: 4000
    command: cargo run
"#;
        let config = Config::parse(yaml).unwrap();
        assert_eq!(config.bind, "127.0.0.1");
        assert_eq!(config.port, 8080);
        assert_eq!(config.domain, "example.com");
        assert_eq!(config.shutdown_grace, 10);
        assert_eq!(config.services.len(), 2);

        let web = &config.services["web"];
        assert_eq!(web.port, 3000);
        assert_eq!(web.command, "npm start");
        assert_eq!(web.idle_timeout, 120);

        let api = &config.services["api"];
        assert_eq!(api.port, 4000);
        assert_eq!(api.idle_timeout, 300); // inherited from global
    }

    #[test]
    fn test_parse_minimal_config() {
        let yaml = r#"
services:
  app:
    port: 5000
    command: python app.py
"#;
        let config = Config::parse(yaml).unwrap();
        assert_eq!(config.bind, "0.0.0.0");
        assert_eq!(config.port, 80);
        assert_eq!(config.domain, "localhost");

        let app = &config.services["app"];
        assert_eq!(app.idle_timeout, 600); // DEFAULT_IDLE_TIMEOUT
    }
}
