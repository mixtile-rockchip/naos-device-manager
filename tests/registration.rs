use naos_core::config::Config;
use serde_json::{Value, json};
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const TOKEN: &str = "test-only-explicit-gateway-token";
const OLD_TOKEN: &str = "test-only-previous-ini-token";
const PRIVATE_RESPONSE: &str = "test-only-private-ddi-response";
const TENANT: &str = "REGISTER_TENANT";
const PREVIOUS_ID: &str = "PREVIOUS_DEVICE_ID";
const REQUEST_DEADLINE: Duration = Duration::from_secs(4);

#[derive(Clone, Copy)]
enum Scenario {
    Applied,
    Pending,
    MissingSocket,
    StaleSocket,
    Rejected,
}

struct Request {
    method: String,
    path: String,
    valid_gateway_header: bool,
    json: bool,
    body: Vec<u8>,
}

fn accept<S, A>(mut accept: impl FnMut() -> io::Result<(S, A)>) -> S {
    let deadline = Instant::now() + REQUEST_DEADLINE;
    loop {
        match accept() {
            Ok((socket, _)) => return socket,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(2));
            }
            Err(_) => panic!("registration fixture did not receive a connection"),
        }
    }
}

fn read_request(stream: &mut impl Read) -> Request {
    let mut reader = BufReader::new(stream.take(4096));
    let mut line = String::new();
    assert!(reader.read_line(&mut line).unwrap() > 0);
    let mut start = line.split_whitespace();
    let method = start.next().unwrap().to_owned();
    let path = start.next().unwrap().to_owned();
    assert_eq!(start.next(), Some("HTTP/1.1"));
    let mut valid_gateway_header = false;
    let mut json = false;
    let mut length = 0;
    loop {
        line.clear();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        let (key, value) = line.split_once(':').unwrap();
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "authorization" => valid_gateway_header = value == format!("GatewayToken {TOKEN}"),
            "content-type" => json = value == "application/json",
            "content-length" => length = value.parse().unwrap(),
            _ => {}
        }
    }
    assert!(length <= 4096);
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Request {
        method,
        path,
        valid_gateway_header,
        json,
        body,
    }
}

fn reply(stream: &mut impl Write, status: &str, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len(),
    ).unwrap();
}

fn run_registration(directory: &Path, server: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_naos-core"))
        .current_dir(directory)
        .args([
            "hawkbit",
            "--config",
            "device.ini",
            "register",
            "--server",
            server,
            "--tenant",
            TENANT,
            "--token",
            TOKEN,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + REQUEST_DEADLINE;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("registration command did not finish before the test deadline");
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn verify_saved(path: &Path, server: &str) -> String {
    let saved = Config::load(path).unwrap();
    assert!(saved.hawkbit.enabled);
    assert_eq!(saved.hawkbit.token_type, "gateway");
    assert_eq!(saved.hawkbit.server_url, server);
    assert_eq!(saved.hawkbit.tenant, TENANT);
    assert!(
        saved.hawkbit.token == TOKEN,
        "explicit gateway credential was not persisted"
    );
    assert!(!saved.hawkbit.controller_id.is_empty());
    assert!(saved.hawkbit.controller_id != PREVIOUS_ID);
    assert_eq!(saved.hawkbit.poll_interval, Duration::from_secs(420));
    assert!(!saved.watchdog.enabled);
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    saved.hawkbit.controller_id
}

fn exercise(scenario: Scenario) {
    let temp = tempfile::tempdir().unwrap();
    let ini = temp.path().join("device.ini");
    let socket = temp.path().join("core.sock");
    let state = temp.path().join("state");
    let original = format!(
        "# Retain settings and comments.\n[core]\nsocket={}\nstate_dir={}\n[hawkbit]\nenabled=false\nserver_url=http://previous.invalid\ntenant=PREVIOUS_TENANT\ncontroller_id={PREVIOUS_ID}\ntoken={OLD_TOKEN}\ntoken_type=target\npoll_interval=7m # Keep these settings.\n[watchdog]\nenabled=false\n",
        socket.display(),
        state.display()
    );
    fs::write(&ini, original.as_bytes()).unwrap();
    let ddi = TcpListener::bind("127.0.0.1:0").unwrap();
    ddi.set_nonblocking(true).unwrap();
    let server = format!("http://{}", ddi.local_addr().unwrap());

    let rejected = matches!(scenario, Scenario::Rejected);
    let ddi = thread::spawn(move || {
        let mut peer = accept(|| ddi.accept());
        peer.set_read_timeout(Some(REQUEST_DEADLINE)).unwrap();
        let request = read_request(&mut peer);
        assert_eq!(request.method, "GET");
        assert!(
            request.valid_gateway_header,
            "DDI request did not use the explicit gateway credential"
        );
        let prefix = format!("/{TENANT}/controller/v1/");
        assert!(
            request.path.starts_with(&prefix),
            "DDI did not use the explicit tenant"
        );
        let identity = request.path[prefix.len()..].to_owned();
        assert!(
            identity
                .strip_prefix("NN")
                .is_some_and(|id| id.len() == 18 && id.bytes().all(|b| b.is_ascii_alphanumeric()))
                || identity
                    .strip_prefix("SN--")
                    .is_some_and(|id| id.len() == 16),
            "DDI did not use a generated identity"
        );
        if rejected {
            reply(
                &mut peer,
                "401 Unauthorized",
                &format!("{PRIVATE_RESPONSE} {TOKEN}"),
            );
        } else {
            reply(
                &mut peer,
                "200 OK",
                r#"{"config":{"polling":{"sleep":"00:00:30"}}}"#,
            );
        }
        identity
    });

    let listener = if matches!(scenario, Scenario::MissingSocket) {
        None
    } else {
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Some(listener)
    };
    let (reload, waiting_listener) = match scenario {
        Scenario::Applied | Scenario::Pending => {
            let listener = listener.unwrap();
            let ini = ini.clone();
            let server = server.clone();
            let worker = thread::spawn(move || {
                let mut peer = accept(|| listener.accept());
                peer.set_read_timeout(Some(REQUEST_DEADLINE)).unwrap();
                let request = read_request(&mut peer);
                assert_eq!(request.method, "POST");
                assert_eq!(request.path, "/v1/hawkbit/reload");
                assert!(request.json);
                let request: Value = serde_json::from_slice(&request.body)
                    .expect("Core reload did not send valid JSON");
                assert!(
                    request == json!({"config_path": ini}),
                    "Core reload must send only the absolute config path"
                );
                let identity = verify_saved(&ini, &server);
                match scenario {
                    Scenario::Applied => {
                        reply(&mut peer, "200 OK", r#"{"applied":true,"pending":false}"#)
                    }
                    Scenario::Pending => reply(
                        &mut peer,
                        "202 Accepted",
                        r#"{"applied":false,"pending":true}"#,
                    ),
                    _ => unreachable!(),
                }
                identity
            });
            (Some(worker), None)
        }
        Scenario::StaleSocket => {
            drop(listener);
            assert!(socket.exists());
            (None, None)
        }
        Scenario::MissingSocket | Scenario::Rejected => (None, listener),
    };

    let output = run_registration(temp.path(), &server);
    let identity = ddi.join().unwrap();
    for log in [&output.stdout, &output.stderr] {
        let log = String::from_utf8_lossy(log);
        assert!(
            ![TOKEN, OLD_TOKEN, PRIVATE_RESPONSE]
                .iter()
                .any(|secret| log.contains(secret)),
            "registration output included authentication material or the remote error body",
        );
    }
    let result: Value = serde_json::from_slice(&output.stdout)
        .expect("registration did not return one JSON result");
    assert_eq!(result["controller_id"], identity);
    assert_eq!(result["server_url"], server);
    assert_eq!(result["registered"], !rejected);
    assert_eq!(result["saved"], !rejected);
    assert_eq!(result["applied"], matches!(scenario, Scenario::Applied));
    assert_eq!(result["pending"], matches!(scenario, Scenario::Pending));
    assert_eq!(result["service_running"], reload.is_some());
    assert!(
        !state.exists(),
        "registration must not create updater runtime state"
    );

    if rejected {
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(result["http_status"], 401);
        assert_eq!(result["error_code"], "UNAUTHORIZED");
        assert!(
            fs::read(&ini).unwrap() == original.as_bytes(),
            "remote rejection changed the INI"
        );
        assert!(
            waiting_listener
                .unwrap()
                .accept()
                .is_err_and(|error| error.kind() == io::ErrorKind::WouldBlock),
            "failed registration should not contact Core",
        );
    } else {
        assert_eq!(output.status.code(), Some(0));
        assert_eq!(result["http_status"], 200);
        assert_eq!(result["polling_sleep"], "30s");
        assert!(result.get("error_code").is_none());
        assert_eq!(verify_saved(&ini, &server), identity);
        if let Some(reload) = reload {
            assert_eq!(reload.join().unwrap(), identity);
        }
        let contents = fs::read_to_string(&ini).unwrap();
        assert!(contents.contains("# Keep these settings."));
        assert!(!contents.contains(OLD_TOKEN));
    }
}

#[test]
fn registration_saves_and_applies_to_running_core() {
    exercise(Scenario::Applied);
}

#[test]
fn registration_reports_pending_core_apply() {
    exercise(Scenario::Pending);
}

#[test]
fn registration_saves_when_core_is_stopped() {
    exercise(Scenario::MissingSocket);
    exercise(Scenario::StaleSocket);
}

#[test]
fn rejected_registration_keeps_ini_and_skips_core_reload() {
    exercise(Scenario::Rejected);
}
