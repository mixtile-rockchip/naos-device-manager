use std::error::Error as _;
use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use crate::config::HawkbitConfig;

const MAX_RESPONSE: usize = 64 * 1024;
const NEUTRAL_DOWNLOAD_PATH: &str = "/var/lib/naos-core/downloads";

pub(super) fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
}

fn auth_token(value: &str) -> bool {
    (8..=1024).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._~+/=-".contains(&byte))
}

pub(super) fn token_authentication(kind: &str) -> Result<(&'static str, &'static str)> {
    match kind {
        "" | "target" => Ok(("TargetToken", "auth_token")),
        "gateway" => Ok(("GatewayToken", "gateway_token")),
        _ => Err(anyhow!("hawkBit token_type must be target or gateway")),
    }
}

fn origin_authority(raw: &str) -> Option<&str> {
    let authority = raw
        .strip_prefix("http://")
        .or_else(|| raw.strip_prefix("https://"))?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    (!authority.is_empty()
        && !authority.contains(['/', '@', '\\', '?', '#'])
        && !authority
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace()))
    .then_some(authority)
}

pub(super) fn validate(
    cfg: &HawkbitConfig,
    poll_interval: Duration,
    timeout: Duration,
    download_path: &str,
) -> Result<Url> {
    token_authentication(&cfg.token_type)?;
    let origin = Url::parse(&cfg.server_url)
        .ok()
        .filter(|url| {
            origin_authority(&cfg.server_url).is_some()
                && url.host().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.path() == "/"
                && url.query().is_none()
                && url.fragment().is_none()
        })
        .ok_or_else(|| {
            anyhow!("hawkBit server must be an HTTP(S) origin without credentials or path")
        })?;
    ensure!(
        identifier(&cfg.tenant)
            && !matches!(cfg.tenant.as_str(), "." | "..")
            && identifier(&cfg.controller_id)
            && !matches!(cfg.controller_id.as_str(), "." | "..")
            && auth_token(&cfg.token),
        "hawkBit tenant, controller identifier or token is invalid"
    );
    ensure!(
        (Duration::from_secs(1)..=Duration::from_secs(24 * 3600)).contains(&poll_interval)
            && (Duration::from_secs(1)..=Duration::from_secs(300)).contains(&timeout),
        "hawkBit polling interval or timeout is outside supported limits"
    );
    ensure!(
        download_path.strip_prefix('/').is_some_and(|suffix| {
            suffix
                .split('/')
                .all(|part| !matches!(part, "" | "." | ".."))
        }) && !download_path.contains(['\\', '\r', '\n', '\0']),
        "hawkBit download directory must be an absolute clean path without INI escapes"
    );
    Ok(origin)
}

pub(super) fn client(timeout: Duration) -> Result<Client> {
    Client::builder()
        .timeout(timeout)
        .pool_idle_timeout(Duration::from_secs(65))
        .pool_max_idle_per_host(1)
        .redirect(reqwest::redirect::Policy::none())
        .min_tls_version(reqwest::tls::Version::TLS_1_2)
        .no_proxy()
        .build()
        .map_err(|_| anyhow!("cannot create hawkBit HTTP client"))
}

pub(super) fn run_with_runtime<F: std::future::Future>(operation: F) -> Result<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(operation);
    // Cancellation also bounds shutdown if a system DNS lookup is stalled.
    runtime.shutdown_timeout(Duration::from_millis(100));
    Ok(result)
}

fn controller_url(origin: &Url, cfg: &HawkbitConfig, config_data: bool) -> Url {
    let mut url = origin.clone();
    let mut path = format!("/{}/controller/v1/{}", cfg.tenant, cfg.controller_id);
    if config_data {
        path.push_str("/configData");
    }
    url.set_path(&path);
    url
}

fn polling_sleep(value: &str) -> Result<Duration> {
    let invalid = || anyhow!("invalid hawkBit polling.sleep");
    let numbers: Vec<_> = value
        .split(':')
        .map(|part| {
            if part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_digit()) {
                part.parse::<u64>().ok()
            } else {
                None
            }
        })
        .collect();
    let [Some(hours), Some(minutes), Some(seconds)] = numbers.as_slice() else {
        return Err(invalid());
    };
    let total = hours * 3600 + minutes * 60 + seconds;
    if *hours > 23 || *minutes > 59 || *seconds > 59 || total == 0 {
        return Err(invalid());
    }
    Ok(Duration::from_secs(total))
}

pub(super) fn duration_text(sleep: Duration) -> String {
    let total = sleep.as_secs();
    let hours = total / 3600;
    let minutes = (total / 60) % 60;
    let seconds = total % 60;
    match (hours, minutes) {
        (0, 0) => format!("{seconds}s"),
        (0, _) => format!("{minutes}m{seconds}s"),
        _ => format!("{hours}h{minutes}m{seconds}s"),
    }
}

#[derive(Default)]
pub(super) struct PollResponse {
    pub(super) status: u16,
    pub(super) sleep: Duration,
    pub(super) action: &'static str,
    pub(super) config_data: bool,
}

pub(super) struct PollError {
    pub(super) status: u16,
    pub(super) code: &'static str,
    pub(super) message: String,
}

fn network_code(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        return "TIMEOUT";
    }
    let mut source = err.source();
    while let Some(cause) = source {
        let cause_text = cause.to_string().to_ascii_lowercase();
        if ["tls", "certificate", "handshake"]
            .iter()
            .any(|text| cause_text.contains(text))
        {
            return "TLS";
        }
        source = cause.source();
    }
    "NETWORK"
}

async fn bounded_body(mut response: Response) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            chunk.len() <= MAX_RESPONSE - body.len(),
            "response too large"
        );
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Deserialize)]
struct DdiResponse {
    config: DdiConfig,
    #[serde(rename = "_links", default)]
    links: Option<std::collections::BTreeMap<String, Value>>,
}

#[derive(Deserialize)]
struct DdiConfig {
    polling: DdiPolling,
}

#[derive(Deserialize)]
struct DdiPolling {
    sleep: String,
}

pub(super) async fn poll(
    client: &Client,
    origin: &Url,
    cfg: &HawkbitConfig,
) -> Result<PollResponse, PollError> {
    let (scheme, _) = token_authentication(&cfg.token_type).map_err(|error| PollError {
        code: "CONFIGURATION",
        status: 0,
        message: error.to_string(),
    })?;
    let response = client
        .get(controller_url(origin, cfg, false))
        .header("Authorization", format!("{scheme} {}", cfg.token))
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| PollError {
            status: 0,
            code: network_code(&error),
            message: "hawkBit heartbeat failed (network, TLS or deadline)".into(),
        })?;
    let status = response.status().as_u16();
    let body = bounded_body(response).await;
    if let Some(error) = body
        .as_ref()
        .err()
        .and_then(|error| error.downcast_ref::<reqwest::Error>())
        && error.is_timeout()
    {
        return Err(PollError {
            status,
            code: "TIMEOUT",
            message: "hawkBit heartbeat failed (network, TLS or deadline)".into(),
        });
    }
    let response_error = |code, message| PollError {
        status,
        code,
        message,
    };
    if status != 200 {
        return Err(response_error(
            match status {
                401 => "UNAUTHORIZED",
                403 => "FORBIDDEN",
                300..=399 => "REDIRECT",
                _ => "HTTP",
            },
            format!("hawkBit heartbeat returned HTTP {status}"),
        ));
    }
    let body = body.map_err(|_| {
        response_error(
            "RESPONSE",
            "hawkBit heartbeat response exceeds limit or cannot be read".into(),
        )
    })?;
    let data: DdiResponse = serde_json::from_slice(&body)
        .map_err(|_| response_error("RESPONSE", "invalid hawkBit heartbeat JSON".into()))?;
    let sleep = polling_sleep(&data.config.polling.sleep)
        .map_err(|error| response_error("RESPONSE", error.to_string()))?;
    let links = data.links.unwrap_or_default();
    let action = if links.contains_key("cancelAction") {
        "cancel"
    } else if links.contains_key("deploymentBase") {
        "deployment"
    } else {
        ""
    };
    Ok(PollResponse {
        status,
        sleep,
        action,
        config_data: links.contains_key("configData"),
    })
}

pub(super) async fn report_attributes(
    client: &Client,
    origin: &Url,
    cfg: &HawkbitConfig,
    attributes: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    use super::attributes::valid_attribute_value;
    ensure!(
        attributes
            .get("system_version")
            .is_some_and(|version| !version.trim().is_empty())
            && attributes.len() <= 16,
        "system version attributes are unavailable"
    );
    ensure!(
        attributes.iter().all(|(key, value)| identifier(key)
            && !value.is_empty()
            && valid_attribute_value(value)),
        "invalid system version attributes"
    );
    let (scheme, _) = token_authentication(&cfg.token_type)?;
    let body = serde_json::to_vec(&serde_json::json!({ "mode": "merge", "data": attributes }))
        .map_err(|_| anyhow!("cannot encode system version attributes"))?;
    let response = client
        .put(controller_url(origin, cfg, true))
        .header("Authorization", format!("{scheme} {}", cfg.token))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| anyhow!("hawkBit attributes request failed (network, TLS or deadline)"))?;
    let success = response.status().is_success();
    bounded_body(response)
        .await
        .map_err(|_| anyhow!("hawkBit attributes response exceeds limit or cannot be read"))?;
    ensure!(success, "hawkBit attributes update was rejected");
    Ok(())
}

pub(super) fn updater_ini(cfg: &HawkbitConfig, origin: &Url) -> Result<Vec<u8>> {
    let (_, token_key) = token_authentication(&cfg.token_type)?;
    let host =
        origin_authority(&cfg.server_url).ok_or_else(|| anyhow!("invalid updater origin"))?;
    let seconds = cfg.timeout.as_secs() + u64::from(cfg.timeout.subsec_nanos() != 0);
    let bundle = Path::new(&cfg.download_path).join("hawkbit.raucb");
    Ok(format!(
        "[client]\nhawkbit_server={host}\ntenant_id={}\ntarget_name={}\n{token_key}={}\nssl={}\nssl_verify=true\nstream_bundle=false\nbundle_download_location={}\npost_update_reboot=false\nconnect_timeout={seconds}\ntimeout={seconds}\n[device]\nsource=naos-core\n",
        cfg.tenant, cfg.controller_id, cfg.token, origin.scheme() == "https", bundle.display()
    ).into_bytes())
}

#[derive(Default, Serialize)]
pub struct CheckResult {
    pub valid: bool,
    pub http_status: u16,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub server_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub controller_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub polling_sleep: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error_code: String,
}

impl CheckResult {
    fn fail(mut self, code: &str, message: impl Into<String>) -> Self {
        self.error_code = code.into();
        self.error = message.into();
        self
    }
}

pub fn check(cfg: &HawkbitConfig) -> CheckResult {
    let mut result = CheckResult::default();
    if cfg.server_url.trim().is_empty() {
        return result.fail("CONFIGURATION", "hawkBit server URL is not configured");
    }
    let timeout = if cfg.timeout.is_zero() {
        Duration::from_secs(30)
    } else {
        cfg.timeout
    };
    let origin = match validate(cfg, Duration::from_secs(1), timeout, NEUTRAL_DOWNLOAD_PATH) {
        Ok(origin) => origin,
        Err(error) => return result.fail("CONFIGURATION", error.to_string()),
    };
    result.server_url = cfg.server_url.trim_end_matches('/').to_owned();
    result.controller_id.clone_from(&cfg.controller_id);
    let response = run_with_runtime(async {
        let client = client(timeout)?;
        Ok::<_, anyhow::Error>(poll(&client, &origin, cfg).await)
    })
    .and_then(|result| result);
    match response {
        Ok(Ok(poll)) => {
            result.valid = true;
            result.http_status = poll.status;
            result.polling_sleep = duration_text(poll.sleep);
            result
        }
        Ok(Err(error)) => {
            result.http_status = error.status;
            let message = match error.code {
                "TIMEOUT" => "hawkBit token check timed out",
                "UNAUTHORIZED" => "hawkBit rejected the token (HTTP 401)",
                "FORBIDDEN" => "hawkBit denied this controller/token (HTTP 403)",
                "REDIRECT" => "hawkBit redirected the request; redirects are not allowed",
                "RESPONSE" => "hawkBit did not return a valid bounded DDI response",
                "TLS" => "hawkBit TLS handshake or certificate verification failed",
                "HTTP" => {
                    return result.fail(
                        "HTTP",
                        format!("hawkBit token check returned HTTP {}", error.status),
                    );
                }
                _ => "cannot connect to the configured hawkBit server",
            };
            result.fail(error.code, message)
        }
        Err(_) => result.fail("NETWORK", "cannot connect to the configured hawkBit server"),
    }
}

#[derive(Default, Serialize)]
pub struct RegisterResult {
    pub registered: bool,
    pub http_status: u16,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub server_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub controller_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub polling_sleep: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error_code: String,
}

pub fn register(cfg: &HawkbitConfig) -> RegisterResult {
    if cfg.token_type != "gateway" {
        return RegisterResult {
            error_code: "CONFIGURATION".into(),
            error: "manual registration requires a GatewayToken".into(),
            ..RegisterResult::default()
        };
    }
    let result = check(cfg);
    RegisterResult {
        registered: result.valid,
        http_status: result.http_status,
        server_url: result.server_url,
        controller_id: result.controller_id,
        polling_sleep: result.polling_sleep,
        error: result.error,
        error_code: result.error_code,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};

    const TOKEN: &str = "synthetic-token";

    pub(super) fn profile(url: String, token_type: &str) -> HawkbitConfig {
        HawkbitConfig {
            enabled: false,
            server_url: url,
            controller_id: "device-1".into(),
            tenant: "DEFAULT".into(),
            token: TOKEN.into(),
            token_type: token_type.into(),
            timeout: Duration::from_secs(2),
            poll_interval: Duration::from_secs(1),
            download_path: NEUTRAL_DOWNLOAD_PATH.into(),
        }
    }

    fn mock(status: u16, data: String) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let join = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = String::new();
            let mut reader = BufReader::new(&mut stream);
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let size = request
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find_map(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or_default();
            if size > 0 {
                let mut body = vec![0; size];
                reader.read_exact(&mut body).unwrap();
                request.push_str("\r\n");
                request.push_str(std::str::from_utf8(&body).unwrap());
            }
            write!(stream, "HTTP/1.1 {status} Mock\r\nLocation: http://127.0.0.1:9/sensitive\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len()).unwrap();
            request
        });
        (url, join)
    }

    #[test]
    fn sleep_is_strict_and_check_keeps_compatible_duration_strings() {
        for bad in [
            "00:00:00", "24:00:00", "00:60:00", "00:00:60", "0:01:00", "00: 0:01",
        ] {
            assert!(polling_sleep(bad).is_err(), "{bad}");
        }
        assert_eq!(duration_text(polling_sleep("01:02:03").unwrap()), "1h2m3s");
        assert_eq!(duration_text(polling_sleep("00:01:00").unwrap()), "1m0s");
    }

    #[test]
    fn configuration_rejects_paths_credentials_and_unsafe_authentication() {
        for url in [
            "https://name:pass@example.com",
            "http://example.com/path/../",
            "http://example.com//",
            "ftp://example.com",
            "https://example.com?",
            "https://example.com/#",
            "https:\\example.com",
            "https://example.com \t",
        ] {
            assert_eq!(
                check(&profile(url.into(), "target")).error_code,
                "CONFIGURATION"
            );
        }
        let result = check(&profile("http://127.0.0.1:9".into(), TOKEN));
        assert_eq!(result.error_code, "CONFIGURATION");
        assert!(!result.error.contains(TOKEN));
    }

    #[test]
    fn token_check_retains_origin_and_uses_matching_token_authentication() {
        for (kind, scheme, key) in [
            ("target", "TargetToken", "auth_token"),
            ("gateway", "GatewayToken", "gateway_token"),
        ] {
            let (url, request) = mock(200, r#"{"config":{"polling":{"sleep":"00:01:00"}},"_links":{"deploymentBase":{"href":"https://elsewhere.invalid/secret"}}}"#.into());
            let cfg = profile(url.clone(), kind);
            let result = check(&cfg);
            assert!(result.valid, "{}", result.error);
            assert_eq!(result.http_status, 200);
            assert_eq!(result.server_url, url);
            assert_eq!(result.polling_sleep, "1m0s");
            let request = request.join().unwrap();
            assert!(request.starts_with("GET /DEFAULT/controller/v1/device-1 HTTP/1.1\r\n"));
            assert!(request.contains(&format!("authorization: {scheme} {TOKEN}\r\n")));
            let ini =
                String::from_utf8(updater_ini(&cfg, &Url::parse(&url).unwrap()).unwrap()).unwrap();
            assert!(ini.contains(&format!("\n{key}={TOKEN}\n")));
            assert!(ini.contains("\npost_update_reboot=false\n"));
            assert!(ini.contains("\nssl_verify=true\n"));
        }
    }

    #[test]
    fn check_rejects_redirects_unexpected_status_and_oversized_response_without_leaking() {
        for (status, code) in [
            (301, "REDIRECT"),
            (401, "UNAUTHORIZED"),
            (403, "FORBIDDEN"),
            (500, "HTTP"),
            (200, "RESPONSE"),
        ] {
            let (url, request) = mock(status, format!("private-response: {TOKEN}"));
            let result = check(&profile(url, "target"));
            assert!(!result.valid);
            assert_eq!(result.http_status, status);
            assert_eq!(result.error_code, code);
            assert!(!result.error.contains(TOKEN));
            assert!(!result.error.contains("private-response"));
            request.join().unwrap();
        }
        let oversized = " ".repeat(MAX_RESPONSE + 1);
        let (url, request) = mock(200, oversized);
        assert_eq!(check(&profile(url, "target")).error_code, "RESPONSE");
        request.join().unwrap();
    }

    #[test]
    fn registration_requires_gateway_credentials() {
        let failed = register(&profile("http://127.0.0.1:9".into(), "target"));
        assert_eq!(failed.error_code, "CONFIGURATION");
        let (url, request) = mock(200, r#"{"config":{"polling":{"sleep":"00:00:01"}}}"#.into());
        let result = register(&profile(url, "gateway"));
        assert!(result.registered, "{}", result.error);
        assert!(
            request
                .join()
                .unwrap()
                .contains("authorization: GatewayToken")
        );
    }

    #[test]
    fn attributes_use_the_configured_origin_and_merge_body() {
        let (url, request) = mock(204, String::new());
        let origin = Url::parse(&url).unwrap();
        let cfg = profile(url, "gateway");
        let attrs = std::collections::BTreeMap::from([
            ("system_name".into(), "NAOS".into()),
            ("system_version".into(), "1.2.3".into()),
        ]);
        run_with_runtime(async {
            let client = client(cfg.timeout).unwrap();
            report_attributes(&client, &origin, &cfg, &attrs)
                .await
                .unwrap();
        })
        .unwrap();
        let request = request.join().unwrap();
        assert!(request.starts_with("PUT /DEFAULT/controller/v1/device-1/configData HTTP/1.1\r\n"));
        assert!(request.contains(&format!("authorization: GatewayToken {TOKEN}\r\n")));
        assert!(request.contains("content-type: application/json\r\n"));
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(body).unwrap(),
            serde_json::json!({"mode": "merge", "data": attrs})
        );
    }

    #[test]
    fn attributes_reject_invalid_data_and_redirects_without_leaking() {
        let (url, request) = mock(307, format!("private-response: {TOKEN}"));
        let origin = Url::parse(&url).unwrap();
        let cfg = profile(url, "target");
        let attrs = std::collections::BTreeMap::from([("system_version".into(), "1.2.3".into())]);
        run_with_runtime(async {
            let client = client(cfg.timeout).unwrap();
            let no_data = report_attributes(&client, &origin, &cfg, &Default::default())
                .await
                .unwrap_err();
            assert_eq!(
                no_data.to_string(),
                "system version attributes are unavailable"
            );
            let error = report_attributes(&client, &origin, &cfg, &attrs)
                .await
                .unwrap_err();
            assert_eq!(error.to_string(), "hawkBit attributes update was rejected");
        })
        .unwrap();
        request.join().unwrap();
    }
}
