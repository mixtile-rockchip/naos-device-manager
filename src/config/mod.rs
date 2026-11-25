use anyhow::{Context, Result, bail, ensure};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

mod ini;
pub use ini::{ensure_controller_id, save_hawkbit_registration};

pub const DEFAULT_PATH: &str = "/etc/naos-core/device.ini";
pub const DEFAULT_SOCKET: &str = "/run/naos/core.sock";

#[derive(Clone, PartialEq, Eq)]
pub struct HawkbitConfig {
    pub enabled: bool,
    pub server_url: String,
    pub tenant: String,
    pub controller_id: String,
    pub token: String,
    pub token_type: String,
    pub poll_interval: Duration,
    pub timeout: Duration,
    pub download_path: String,
}

impl std::fmt::Debug for HawkbitConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Authentication data should not escape through a debug print of Config.
        formatter
            .debug_struct("HawkbitConfig")
            .field("enabled", &self.enabled)
            .field("token_type", &self.token_type)
            .field("poll_interval", &self.poll_interval)
            .field("timeout", &self.timeout)
            .field("download_path", &self.download_path)
            .finish_non_exhaustive()
    }
}

impl Default for HawkbitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: String::new(),
            tenant: String::new(),
            controller_id: String::new(),
            token: String::new(),
            token_type: "target".into(),
            poll_interval: Duration::from_secs(240),
            timeout: Duration::from_secs(30),
            download_path: "/var/lib/naos-core/downloads".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchdogConfig {
    pub enabled: bool,
    pub device: String,
    pub timeout: Duration,
    pub feed_interval: Duration,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            device: "/dev/watchdog0".into(),
            timeout: Duration::from_secs(30),
            feed_interval: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub soc: String,
    pub http_enabled: bool,
    pub http_listen: String,
    pub socket: String,
    pub container_socket: String,
    pub container_enabled: bool,
    pub container_manifest: String,
    pub boot_check_enabled: bool,
    pub boot_check_program: String,
    pub state_dir: String,
    pub hawkbit: HawkbitConfig,
    pub watchdog: WatchdogConfig,
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            soc: String::new(),
            http_enabled: true,
            http_listen: "127.0.0.1:8890".into(),
            socket: DEFAULT_SOCKET.into(),
            container_socket: "/var/run/naosd.sock".into(),
            container_enabled: false,
            container_manifest: "/userdata/manifest.yml".into(),
            boot_check_enabled: false,
            boot_check_program: String::new(),
            state_dir: "/var/lib/naos-core".into(),
            hawkbit: HawkbitConfig::default(),
            watchdog: WatchdogConfig::default(),
            warnings: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let document = ini::read(path)?;
        let mut config = Self::from_ini(path, &document);
        if config.watchdog.feed_interval >= config.watchdog.timeout {
            config.warn(
                path,
                "watchdog feed_interval must be shorter than timeout; timing overrides skipped",
            );
            let defaults = WatchdogConfig::default();
            config.watchdog.feed_interval = defaults.feed_interval;
            config.watchdog.timeout = defaults.timeout;
        }
        if config.hawkbit.enabled
            && (config.hawkbit.server_url.is_empty()
                || config.hawkbit.tenant.is_empty()
                || config.hawkbit.token.is_empty())
        {
            config.warn(
                path,
                "enabled hawkbit requires server_url, tenant and token; hawkbit disabled",
            );
            config.hawkbit.enabled = false;
        }
        if config.container_enabled && config.container_manifest.is_empty() {
            config.warn(
                path,
                "enabled container requires manifest; container startup disabled",
            );
            config.container_enabled = false;
        }
        if config.boot_check_enabled && config.boot_check_program.is_empty() {
            config.warn(
                path,
                "boot.check_program: required when boot.check_enabled is true",
            );
        }
        Ok(config)
    }

    fn from_ini(path: &Path, document: &ini::Document) -> Self {
        let mut config = Self::default();
        for (section, fields) in document.settings() {
            if section == "DEFAULT" && fields.is_empty() {
                continue;
            }
            if !matches!(
                section,
                "platform" | "http" | "core" | "container" | "boot" | "hawkbit" | "watchdog"
            ) {
                config.warn(path, "unknown configuration section");
                continue;
            }
            for (key, value) in fields {
                if let Err(error) = config.assign(section, key, value) {
                    config.warn(path, &error.to_string());
                }
            }
        }
        config
    }

    fn warn(&mut self, path: &Path, message: &str) {
        self.warnings.push(format!("{}: {message}", path.display()));
    }

    fn assign(&mut self, section: &str, key: &str, value: &str) -> Result<()> {
        let mut apply = || -> Result<bool> {
            match (section, key) {
                ("platform", "soc") => {
                    ensure!(identifier(value, 64), "invalid platform identifier");
                    value.clone_into(&mut self.soc);
                }
                ("http", "enabled") => self.http_enabled = boolean(value)?,
                ("http", "listen") => {
                    let valid = value.parse::<SocketAddr>().is_ok_and(|address| {
                        address.ip().to_canonical().is_loopback() && address.port() != 0
                    });
                    ensure!(
                        valid,
                        "expected a numeric loopback address and port 1..65535"
                    );
                    value.clone_into(&mut self.http_listen);
                }
                ("core", "socket") => self.socket = absolute_path(value, 107)?,
                ("core", "state_dir") => self.state_dir = absolute_path(value, 4096)?,
                ("container", "address") => self.container_socket = absolute_path(value, 107)?,
                ("container", "enabled") => self.container_enabled = boolean(value)?,
                ("container", "manifest") => {
                    ensure!(
                        value.is_empty() || crate::container::valid_manifest_path(value),
                        "expected a clean /userdata/ path to manifest.yml or /var/lib/naos/containers/<id>/manifest.yml within the length limit"
                    );
                    value.clone_into(&mut self.container_manifest);
                }
                ("boot", "check_enabled") => self.boot_check_enabled = boolean(value)?,
                ("boot", "check_program") if value.is_empty() => self.boot_check_program.clear(),
                ("boot", "check_program") => self.boot_check_program = absolute_path(value, 4096)?,
                ("watchdog", "enabled") => self.watchdog.enabled = boolean(value)?,
                ("watchdog", "device") => {
                    ensure!(
                        value
                            .strip_prefix("/dev/watchdog")
                            .is_some_and(|number| !number.is_empty()
                                && number.bytes().all(|byte| byte.is_ascii_digit())),
                        "expected an explicit /dev/watchdogN node"
                    );
                    value.clone_into(&mut self.watchdog.device);
                }
                ("watchdog", "timeout") => self.watchdog.timeout = duration(value, 300, true)?,
                ("watchdog", "feed_interval") => {
                    self.watchdog.feed_interval = duration(value, 60, false)?
                }
                ("hawkbit", "enabled") => self.hawkbit.enabled = boolean(value)?,
                ("hawkbit", "server_url") => self.hawkbit.server_url = server_url(value)?,
                ("hawkbit", "tenant" | "controller_id") => {
                    ensure!(identifier(value, 128), "invalid device identity");
                    if key == "tenant" {
                        value.clone_into(&mut self.hawkbit.tenant);
                    } else {
                        value.clone_into(&mut self.hawkbit.controller_id);
                    }
                }
                ("hawkbit", "token") => {
                    ensure!(value.len() <= 1024, "credential exceeds length limit");
                    ensure!(
                        value.bytes().all(|byte| byte.is_ascii_graphic()),
                        "credential must use visible ASCII without whitespace"
                    );
                    value.clone_into(&mut self.hawkbit.token);
                }
                ("hawkbit", "token_type") => {
                    ensure!(
                        matches!(value, "target" | "gateway"),
                        "expected target or gateway"
                    );
                    value.clone_into(&mut self.hawkbit.token_type);
                }
                ("hawkbit", "poll_interval") => {
                    self.hawkbit.poll_interval = duration(value, 86400, false)?
                }
                ("hawkbit", "timeout") => self.hawkbit.timeout = duration(value, 300, false)?,
                ("hawkbit", "download_path") => {
                    self.hawkbit.download_path = absolute_path(value, 4096)?
                }
                _ => return Ok(false),
            }
            Ok(true)
        };
        match apply() {
            Ok(true) => Ok(()),
            Ok(false) => bail!("unknown configuration key"),
            // Both the key and our error text are from the fixed schema.
            Err(error) => bail!("{section}.{key}: {error}"),
        }
    }
}

fn identifier(value: &str, maximum: usize) -> bool {
    value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.@".contains(&byte))
}

fn boolean(value: &str) -> Result<bool> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("expected true or false"),
    }
}

fn absolute_path(value: &str, maximum: usize) -> Result<String> {
    ensure!(
        value.len() <= maximum
            && !value.contains(['\0', '\n', '\r'])
            && value
                .strip_prefix('/')
                .is_some_and(|path| path.split('/').all(|part| !matches!(part, "" | "." | ".."))),
        "expected a clean absolute path within the length limit"
    );
    Ok(value.into())
}

fn server_url(value: &str) -> Result<String> {
    if value.is_empty() {
        return Ok(String::new());
    }
    let authority = value
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or_default());
    let parsed = url::Url::parse(value);
    let valid = value.len() <= 2048
        && !value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        && authority.is_some_and(|authority| !authority.is_empty() && !authority.contains('@'))
        && parsed.is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        });
    ensure!(
        valid,
        "expected an HTTP(S) server URL without credentials, query or fragment"
    );
    Ok(value.trim_end_matches('/').into())
}

fn duration(value: &str, maximum_seconds: u64, whole_seconds: bool) -> Result<Duration> {
    let duration = parse_duration(value)
        .filter(|duration| {
            *duration >= Duration::from_secs(1) && *duration <= Duration::from_secs(maximum_seconds)
        })
        .filter(|duration| !whole_seconds || duration.subsec_nanos() == 0);
    duration.context("duration is outside the supported range")
}

/// Parse the Go duration syntax used in device.ini, such as `1m30.5s`.
fn parse_duration(value: &str) -> Option<Duration> {
    let mut rest = value.strip_prefix('+').unwrap_or(value);
    let mut total: u64 = 0;
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let whole_end = rest
            .as_bytes()
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let (whole, suffix) = rest.split_at(whole_end);
        let (fraction, suffix) = if let Some(suffix) = suffix.strip_prefix('.') {
            let count = suffix
                .as_bytes()
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            suffix.split_at(count)
        } else {
            ("", suffix)
        };
        if whole.is_empty() && fraction.is_empty() {
            return None;
        }
        let (unit, multiplier) = [
            ("ns", 1),
            ("us", 1000),
            ("µs", 1000),
            ("μs", 1000),
            ("ms", 1_000_000),
            ("s", 1_000_000_000),
            ("m", 60_000_000_000),
            ("h", 3_600_000_000_000),
        ]
        .into_iter()
        .find(|(unit, _)| suffix.starts_with(unit))?;
        rest = suffix.strip_prefix(unit)?;
        let integer: u64 = if whole.is_empty() {
            0
        } else {
            whole.parse().ok()?
        };
        let leading_fraction = &fraction[..fraction.len().min(18)];
        let fraction_nanos = if leading_fraction.is_empty() {
            0
        } else {
            let scaled = leading_fraction.parse::<u128>().ok()? * u128::from(multiplier);
            (scaled / 10_u128.pow(leading_fraction.len() as u32)) as u64
        };
        total = total
            .checked_add(integer.checked_mul(multiplier)?)?
            .checked_add(fraction_nanos)?;
    }
    Some(Duration::from_nanos(total))
}

#[cfg(test)]
mod tests;
