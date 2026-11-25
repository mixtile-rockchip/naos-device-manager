use serde_json::Value;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn get(path: &Path, route: &str) -> std::io::Result<String> {
    let mut socket = UnixStream::connect(path)?;
    socket.set_read_timeout(Some(Duration::from_secs(1)))?;
    socket.write_all(
        format!("GET {route} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    let mut result = String::new();
    socket.read_to_string(&mut result)?;
    Ok(result)
}

fn await_daemon(socket: &Path, daemon: &mut Daemon) {
    let latest = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(response) = get(socket, "/healthz") {
            assert!(response.starts_with("HTTP/1.1 200 OK"));
            assert!(response.contains(r#"{"status":"ok"}"#));
            return;
        }
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "test service exited before listening"
        );
        assert!(Instant::now() < latest, "test service did not become ready");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn binary_serves_uses_unix_cli_and_shuts_down_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let ini = dir.path().join("device.ini");
    let socket = dir.path().join("core.sock");
    let state = dir.path().join("state");
    fs::write(
        &ini,
        format!(
            "[core]\nsocket={}\nstate_dir={}\n[http]\nenabled=false\n[watchdog]\nenabled=false\n",
            socket.display(),
            state.display()
        ),
    )
    .unwrap();

    let binary = env!("CARGO_BIN_EXE_naos-core");
    let mut daemon = Daemon(
        Command::new(binary)
            .arg("--config")
            .arg(&ini)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    await_daemon(&socket, &mut daemon);
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for (arguments, key, expected) in [
        (vec!["tasks"], "tasks", serde_json::json!([])),
        (
            vec!["hawkbit", "status"],
            "enabled",
            serde_json::json!(false),
        ),
    ] {
        let result = Command::new(binary)
            .arg("--config")
            .arg(&ini)
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let json: Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(json[key], expected);
        assert!(result.stdout.ends_with(b"\n"));
    }
    // Signal the test process only. This uses the daemon's normal OpenRC stop path.
    assert_eq!(
        unsafe { libc::kill(daemon.0.id() as i32, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = daemon.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "test service did not stop");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!socket.exists());
    assert_eq!(
        fs::metadata(state).unwrap().permissions().mode() & 0o777,
        0o700
    );
}
