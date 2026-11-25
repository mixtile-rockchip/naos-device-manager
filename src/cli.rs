use crate::api::clean_absolute;
use crate::config::{self, Config, HawkbitConfig};
use crate::device::{self, Oem, OemError};
use crate::hawkbit::{self, CheckResult, RegisterResult};
use crate::server::{UnixClient, read_response, socket_unavailable};
use anyhow::{Result, ensure};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

struct Options {
    command: String,
    config: String,
    server: String,
    tenant: String,
    token: String,
    request_id: String,
    args: Vec<String>,
    help: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            command: String::new(),
            config: config::DEFAULT_PATH.into(),
            server: String::new(),
            tenant: String::new(),
            token: String::new(),
            request_id: String::new(),
            args: Vec::new(),
            help: false,
        }
    }
}

fn parse(args: &[OsString], options: &mut Options) -> Result<(), ()> {
    let mut args = args.iter();
    let mut literal = false;
    let mut help_command = false;
    while let Some(arg) = args.next() {
        let arg = arg.to_str().ok_or(())?;
        if !literal && arg == "--" {
            literal = true;
            continue;
        }
        if !literal && arg.starts_with('-') && arg != "-" {
            let flag = arg
                .strip_prefix("--")
                .or_else(|| arg.strip_prefix('-'))
                .unwrap();
            let (name, value) = flag
                .split_once('=')
                .map_or((flag, None), |(name, value)| (name, Some(value)));
            if matches!(name, "h" | "help") {
                options.help = match value.unwrap_or("true") {
                    "1" | "t" | "T" | "true" | "TRUE" | "True" => true,
                    "0" | "f" | "F" | "false" | "FALSE" | "False" => false,
                    _ => return Err(()),
                };
                continue;
            }
            let target = match (options.command.as_str(), name) {
                (_, "config") => &mut options.config,
                ("update", "request-id") => &mut options.request_id,
                ("hawkbit register", "server") => &mut options.server,
                ("hawkbit register", "tenant") => &mut options.tenant,
                ("hawkbit register", "token") => &mut options.token,
                _ => return Err(()),
            };
            *target = match value {
                Some(value) => value,
                None => args.next().and_then(|arg| arg.to_str()).ok_or(())?,
            }
            .to_owned();
            continue;
        }
        if !literal && options.args.is_empty() && matches!(arg, "h" | "help") {
            help_command = true;
            continue;
        }
        match options.command.as_str() {
            "" if matches!(
                arg,
                "serve"
                    | "oem"
                    | "containers"
                    | "reboot"
                    | "recovery"
                    | "update"
                    | "tasks"
                    | "task"
                    | "hawkbit"
                    | "boot-check"
            ) =>
            {
                options.command = arg.to_owned()
            }
            "containers" if matches!(arg, "list" | "start" | "stop" | "restart") => {
                options.command = format!("containers {arg}")
            }
            "hawkbit" if matches!(arg, "status" | "check" | "register") => {
                options.command = format!("hawkbit {arg}")
            }
            "" | "containers" | "hawkbit" => return Err(()),
            _ if help_command => return Err(()),
            _ => options.args.push(arg.to_owned()),
        }
    }
    options.help |= help_command;
    if options.help {
        return Ok(());
    }
    let valid = match options.command.as_str() {
        "task" | "containers start" | "containers stop" | "containers restart" => {
            options.args.len() == 1
        }
        "update" => {
            options.args.len() == 1 || (options.args.is_empty() && options.request_id.is_empty())
        }
        _ => options.args.is_empty(),
    };
    valid.then_some(()).ok_or(())
}

pub fn run(args: &[OsString], out: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    let mut options = Options::default();
    if parse(args, &mut options).is_err() {
        let _ = writeln!(
            stderr,
            "naos-core: invalid arguments; see command usage below"
        );
        let _ = help(&options.command, out);
        return 2;
    }
    if options.help || matches!(options.command.as_str(), "" | "hawkbit") {
        return if help(&options.command, out).is_ok() {
            0
        } else {
            1
        };
    }
    if options.command == "hawkbit register" {
        return register(options, out, stderr);
    }
    let cfg = match Config::load(&options.config) {
        Ok(cfg) => cfg,
        Err(error) => {
            let _ = writeln!(stderr, "naos-core: {error}");
            return 1;
        }
    };
    let outcome = match options.command.as_str() {
        "serve" => crate::service::run(cfg, Path::new(&options.config)),
        "oem" => match Oem::read(&cfg.soc) {
            Ok(otp) => {
                let oem = otp.info();
                writeln!(out, "SN: {}\nMAC: {}", oem.sn, oem.mac).map_err(Into::into)
            }
            Err(OemError::Unsupported) => Err(anyhow::anyhow!(
                "OEM is not supported; requires [platform] soc=rk3588"
            )),
            Err(error) => Err(anyhow::anyhow!("read OEM OTP: {error}")),
        },
        "boot-check" => {
            if cfg
                .warnings
                .iter()
                .any(|warning| warning.contains(": boot.check_"))
            {
                Err(anyhow::anyhow!("invalid boot check configuration"))
            } else if cfg.boot_check_enabled {
                device::boot_check(
                    Path::new(&cfg.boot_check_program),
                    Duration::from_secs(60),
                    out,
                    stderr,
                )
                .map_err(|error| anyhow::anyhow!("boot check: {error}"))
            } else {
                Ok(())
            }
        }
        "hawkbit check" => return check(cfg, &options.config, out, stderr),
        _ => return call(&cfg, &options, out, stderr),
    };
    match outcome {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(stderr, "naos-core: {error}");
            1
        }
    }
}

fn check(mut cfg: Config, path: &str, out: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    let result = if cfg.hawkbit.controller_id.is_empty() {
        match config::ensure_controller_id(path, || device::controller_id(&cfg.soc, "")) {
            Ok(id) => {
                cfg.hawkbit.controller_id = id;
                hawkbit::check(&cfg.hawkbit)
            }
            Err(_) => CheckResult {
                error_code: "IDENTITY".into(),
                error: "cannot read or persist the hawkBit controller ID".into(),
                ..CheckResult::default()
            },
        }
    } else {
        hawkbit::check(&cfg.hawkbit)
    };
    json_line(out, stderr, &result, result.valid)
}

#[derive(Default, Serialize)]
struct Registration {
    #[serde(flatten)]
    remote: RegisterResult,
    saved: bool,
    applied: bool,
    pending: bool,
    service_running: bool,
}

fn register(options: Options, out: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    if [&options.server, &options.tenant, &options.token]
        .iter()
        .any(|part| part.trim().is_empty())
    {
        let _ = writeln!(
            stderr,
            "naos-core: register requires --server, --tenant and --token (GatewayToken)"
        );
        let _ = help("hawkbit", stderr);
        return 2;
    }
    let mut response = Registration::default();
    let Ok(id) = device::fresh_controller_id() else {
        response.remote.error_code = "IDENTITY".into();
        response.remote.error = "cannot generate the hawkBit controller ID".into();
        return json_line(out, stderr, &response, false);
    };
    let profile = HawkbitConfig {
        enabled: true,
        server_url: options.server,
        tenant: options.tenant,
        token: options.token,
        token_type: "gateway".into(),
        controller_id: id,
        ..HawkbitConfig::default()
    };
    response.remote = hawkbit::register(&profile);
    if !response.remote.registered {
        return json_line(out, stderr, &response, false);
    }
    let path = Path::new(&options.config);
    if config::save_hawkbit_registration(path, &profile).is_err() {
        response.remote.error_code = "PERSISTENCE".into();
        response.remote.error =
            "device registered, but its configuration could not be saved".into();
        return json_line(out, stderr, &response, false);
    }
    response.saved = true;
    if apply_registration(path, &mut response).is_err() {
        response.remote.error_code = "APPLY".into();
        response.remote.error =
            "device registered and configuration saved, but the running Core could not apply it"
                .into();
        return json_line(out, stderr, &response, false);
    }
    json_line(out, stderr, &response, true)
}

fn apply_registration(path: &Path, response: &mut Registration) -> Result<()> {
    let cfg = Config::load(path)?;
    ensure!(cfg.hawkbit.enabled, "cannot load saved registration");
    let absolute = clean_absolute(&std::path::absolute(path)?);
    let client = UnixClient::new(Path::new(&cfg.socket), Duration::from_secs(10))?;
    let data = json!({"config_path": absolute});
    let reply = match client.request(Method::POST, "/v1/hawkbit/reload", Some(&data)) {
        Ok(reply) => reply,
        Err(error) if socket_unavailable(&error) => return Ok(()),
        Err(error) => {
            response.service_running = true;
            return Err(error.into());
        }
    };
    response.service_running = true;
    let status = reply.status();
    ensure!(
        matches!(status, StatusCode::OK | StatusCode::ACCEPTED),
        "Core rejected registration reload"
    );
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Applied {
        applied: bool,
        pending: bool,
    }
    let Applied { applied, pending } = serde_json::from_slice(&read_response(reply, 4096)?)?;
    ensure!(
        applied != pending && (status == StatusCode::OK) == applied,
        "inconsistent Core reload response"
    );
    response.applied = applied;
    response.pending = pending;
    Ok(())
}

fn json_line(
    out: &mut dyn Write,
    stderr: &mut dyn Write,
    value: &impl Serialize,
    success: bool,
) -> u8 {
    if serde_json::to_writer(&mut *out, value).is_err() || writeln!(out).is_err() {
        let _ = writeln!(stderr, "naos-core: cannot write JSON result");
        return 1;
    }
    u8::from(!success)
}

fn call(cfg: &Config, options: &Options, out: &mut dyn Write, stderr: &mut dyn Write) -> u8 {
    let (method, path, data) = request(options);
    let result = (|| -> Result<bool> {
        let timeout = if matches!(
            options.command.as_str(),
            "containers stop" | "containers restart"
        ) {
            crate::container::GRACEFUL_CONTROL_TIMEOUT + Duration::from_secs(5)
        } else {
            Duration::from_secs(10)
        };
        let client = UnixClient::new(Path::new(&cfg.socket), timeout)?;
        let response = client.request(method, &path, data.as_ref())?;
        let success = response.status().is_success();
        let body = read_response(response, 2 * 1024 * 1024)?;
        out.write_all(&body)?;
        if !body.is_empty() && !body.ends_with(b"\n") {
            writeln!(out)?;
        }
        Ok(success)
    })();
    match result {
        Ok(success) => u8::from(!success),
        Err(error) => {
            let _ = writeln!(stderr, "naos-core: {error}");
            1
        }
    }
}

fn request(options: &Options) -> (Method, String, Option<Value>) {
    let command = options.command.as_str();
    match command {
        "reboot" | "recovery" => (Method::POST, format!("/v1/{command}"), None),
        "update" if !options.args.is_empty() => (
            Method::POST,
            "/v1/update".into(),
            Some(json!({
                "path": options.args[0], "request_id": options.request_id
            })),
        ),
        "tasks" => (Method::GET, "/v1/update-tasks".into(), None),
        "task" => (
            Method::GET,
            format!("/v1/update-tasks/{}", escape(&options.args[0])),
            None,
        ),
        "containers start" | "containers stop" | "containers restart" => (
            Method::POST,
            format!(
                "/v1/containers/{}/{}",
                escape(&options.args[0]),
                &command["containers ".len()..]
            ),
            None,
        ),
        "containers list" | "containers" => (Method::GET, "/v1/containers".into(), None),
        "hawkbit status" => (Method::GET, "/v1/hawkbit".into(), None),
        _ => (Method::GET, format!("/v1/{command}"), None),
    }
}

fn escape(value: &str) -> impl std::fmt::Display + '_ {
    utf8_percent_encode(value, NON_ALPHANUMERIC)
}

fn help(topic: &str, out: &mut dyn Write) -> std::io::Result<()> {
    let (purpose, usage, children, options) = match topic {
        "" => (
            "Manage the NAOS device and its local Core service",
            " COMMAND [arguments]",
            "   serve       Run the Core service in the foreground\n   oem         Read device SN and MAC\n   boot-check  Run the configured boot check\n   containers  List or control containers\n   reboot      Request a reboot\n   recovery    Erase userdata on next boot and reboot\n   update      Install a signed RAUC bundle or show progress\n   tasks       List update tasks\n   task        Show one update task\n   hawkbit     Register, inspect status or check a token\n",
            "",
        ),
        "serve" => (
            "Run the Core service in the foreground (managed by OpenRC)",
            "",
            "",
            "",
        ),
        "oem" => (
            "Read the device SN and MAC from supported OEM OTP",
            "",
            "",
            "",
        ),
        "boot-check" => (
            "Run the configured boot check before OpenRC confirms the slot",
            "",
            "",
            "",
        ),
        "containers" => (
            "List or control containers through naosd",
            " [COMMAND [ID]]",
            "   list     List containers\n   start    Start a container\n   stop     Stop a container\n   restart  Restart a container\n",
            "",
        ),
        "containers list" => ("List the current containers", "", "", ""),
        "containers start" => ("Start the named container", " ID", "", ""),
        "containers stop" => ("Stop the named container", " ID", "", ""),
        "containers restart" => ("Restart the named container", " ID", "", ""),
        "reboot" => ("Request a device reboot", "", "", ""),
        "recovery" => (
            "Erase all userdata during the next boot and reboot",
            "",
            "",
            "",
        ),
        "update" => (
            "Install a local signed RAUC bundle or show update progress",
            " [BUNDLE_PATH]",
            "",
            "   --request-id ID  Idempotency ID for a local update (requires BUNDLE_PATH)\n",
        ),
        "tasks" => ("List persistent update tasks", "", "", ""),
        "task" => ("Show one persistent update task", " TASK_ID", "", ""),
        "hawkbit" => (
            "Register, inspect status or verify a token",
            " COMMAND [options]",
            "   status    Show updater status and logs\n   check     Verify the configured token\n   register  Register with a GatewayToken and persist the profile\n",
            "",
        ),
        "hawkbit status" => ("Show updater status and logs", "", "", ""),
        "hawkbit check" => (
            "Verify the configured token with an authenticated DDI request",
            "",
            "",
            "",
        ),
        "hawkbit register" => (
            "Register with a GatewayToken and apply the stored profile",
            " --server URL --tenant TENANT --token TOKEN",
            "",
            "   --server URL     hawkBit server origin URL\n   --tenant TENANT  hawkBit tenant\n   --token TOKEN    Shared GatewayToken\n",
        ),
        _ => return Ok(()),
    };
    let command = if topic.is_empty() {
        "naos-core".to_owned()
    } else {
        format!("naos-core {topic}")
    };
    write!(
        out,
        "NAME:\n   {command} - {purpose}\n\nUSAGE:\n   {command} [options]{usage}\n"
    )?;
    if !children.is_empty() {
        write!(out, "\nCOMMANDS:\n{children}")?;
    }
    write!(
        out,
        "\nOPTIONS:\n{options}   --config PATH    Device INI (default: {})\n   --help, -h       Show command help\n",
        config::DEFAULT_PATH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> (u8, String, String) {
        let mut out = Vec::new();
        let mut stderr = Vec::new();
        let code = run(
            &args.iter().map(OsString::from).collect::<Vec<_>>(),
            &mut out,
            &mut stderr,
        );
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(stderr).unwrap(),
        )
    }

    #[test]
    fn help_and_usage_errors_precede_io_and_never_echo_secret_flags() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().to_str().unwrap();
        let (code, output, errors) = command(&["help", "hawkbit", "register", "--config", config]);
        assert_eq!(code, 0);
        assert!(output.contains("GatewayToken") && errors.is_empty());
        let (code, output, errors) = command(&[
            "hawkbit",
            "register",
            "--token",
            "secret",
            "--invalid",
            config,
        ]);
        assert_eq!(code, 2);
        assert!(errors.contains("invalid arguments"));
        assert!(!format!("{output}{errors}").contains("secret"));
    }

    #[test]
    fn interspersed_flags_and_escaped_route_are_compatible() {
        let mut options = Options::default();
        parse(
            &[
                "--config",
                "/tmp/device.ini",
                "containers",
                "start",
                "--",
                "id/with/slashes",
            ]
            .iter()
            .map(OsString::from)
            .collect::<Vec<_>>(),
            &mut options,
        )
        .unwrap();
        assert_eq!(options.config, "/tmp/device.ini");
        let (method, uri, body) = request(&options);
        assert_eq!(method, Method::POST);
        assert_eq!(uri, "/v1/containers/id%2Fwith%2Fslashes/start");
        assert!(body.is_none());
    }

    #[test]
    fn init_script_boot_check_does_not_accept_invalid_opt_in() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(temp.path(), "[boot]\ncheck_enabled=yes\n").unwrap();
        let (code, _, errors) = command(&["--config", temp.path().to_str().unwrap(), "boot-check"]);
        assert_eq!(code, 1);
        assert!(errors.contains("invalid boot check configuration"));
    }
}
