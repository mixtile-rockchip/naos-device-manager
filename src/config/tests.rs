use super::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::sync::atomic::{AtomicUsize, Ordering};

fn load(data: &str) -> Result<Config> {
    let temporary = tempfile::NamedTempFile::new()?;
    fs::write(temporary.path(), data)?;
    Config::load(temporary.path())
}

fn profile() -> HawkbitConfig {
    HawkbitConfig {
        server_url: "https://updates.example.test/".into(),
        tenant: "NAOS".into(),
        controller_id: "example-controller".into(),
        token: "private#part;part%(value)s".into(),
        token_type: "gateway".into(),
        ..HawkbitConfig::default()
    }
}

#[test]
fn missing_configuration_uses_defaults_and_only_reads_selected_file() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("device.ini"),
        "[platform]\nsoc=should-not-be-read",
    )
    .unwrap();
    assert_eq!(
        Config::load(directory.path().join("chosen.ini")).unwrap(),
        Config::default()
    );
}

#[test]
fn shipped_fields_and_independent_overrides() {
    let config = load(
        "\
[platform]
soc=rk3588
[http]
enabled=false
listen=[::1]:8891
[core]
socket=/run/naos-custom/core.sock
state_dir=/var/lib/custom-core
[container]
enabled=true
address=/tmp/naosd.sock
manifest=/var/lib/naos/containers/demo/manifest.yml
[boot]
check_enabled=true
check_program=/usr/bin/ready
[hawkbit]
enabled=true
server_url=https://updates.example.test/
tenant=DEFAULT
controller_id=
token=private#part;part
poll_interval=1m15s
timeout=12s
download_path=/userdata/downloads
[watchdog]
enabled=true
device=/dev/watchdog12
timeout=45s
feed_interval=1.5s
",
    )
    .unwrap();
    assert_eq!(config.soc, "rk3588");
    assert!(!config.http_enabled);
    assert_eq!(config.http_listen, "[::1]:8891");
    assert_eq!(config.socket, "/run/naos-custom/core.sock");
    assert_eq!(config.state_dir, "/var/lib/custom-core");
    assert!(config.container_enabled);
    assert_eq!(config.container_socket, "/tmp/naosd.sock");
    assert_eq!(
        config.container_manifest,
        "/var/lib/naos/containers/demo/manifest.yml"
    );
    assert!(config.boot_check_enabled);
    assert_eq!(config.boot_check_program, "/usr/bin/ready");
    assert!(config.hawkbit.enabled);
    assert_eq!(config.hawkbit.server_url, "https://updates.example.test");
    assert_eq!(config.hawkbit.download_path, "/userdata/downloads");
    assert_eq!(config.hawkbit.token, "private#part;part");
    assert!(config.hawkbit.controller_id.is_empty());
    assert_eq!(config.hawkbit.poll_interval, Duration::from_secs(75));
    assert_eq!(config.hawkbit.timeout, Duration::from_secs(12));
    assert!(config.watchdog.enabled);
    assert_eq!(config.watchdog.device, "/dev/watchdog12");
    assert_eq!(config.watchdog.timeout, Duration::from_secs(45));
    assert_eq!(config.watchdog.feed_interval, Duration::from_millis(1500));
    assert!(config.warnings.is_empty(), "{:?}", config.warnings);
}

#[test]
fn last_key_wins_and_invalid_override_restores_builtin_default() {
    let config = load(
        "\
[platform]
soc=rk3588
soc=invalid secret
[hawkbit]
controller_id=first
[hawkbit]
controller_id=last
token=literal%(value)s#part;part
[core]
state_dir=/var/lib/custom
",
    )
    .unwrap();
    assert!(config.soc.is_empty());
    assert_eq!(config.hawkbit.controller_id, "last");
    assert_eq!(config.hawkbit.token, "literal%(value)s#part;part");
    assert_eq!(config.state_dir, "/var/lib/custom");
    assert_eq!(config.warnings.len(), 1);
    assert!(!config.warnings[0].contains("secret"));
}

#[test]
fn malformed_ini_and_unknown_names_do_not_disclose_credentials() {
    let marker = "private-token-marker";
    let error = load(&format!("[hawkbit]\nnot-an-assignment {marker}")).unwrap_err();
    assert!(error.to_string().contains("INI syntax"));
    assert!(!format!("{error:#}").contains(marker));

    let config = load(&format!(
        "[{marker}]\nunknown=value\n[hawkbit]\n{marker}=secret\ntoken={marker}\n"
    ))
    .unwrap();
    assert_eq!(config.warnings.len(), 2);
    assert_eq!(config.hawkbit.token, marker);
    assert!(!format!("{config:?}").contains(marker));
}

#[test]
fn go_ini_supports_quoted_keys_continuations_and_multiline_values() {
    let input = concat!(
        "[platform]\n",
        "\"soc\": rk3588\n",
        "[hawkbit]\n",
        "poll_interval=1m\\\n",
        "  30s\n",
        "token=`ordinary`word` # keeps the interior delimiter\n",
        "[comments]\n",
        "value=\"\"\"first\nsecond\"\"\"\n",
        "[watchdog]\n",
        "enabled='false'\n",
    );
    let config = load(input).unwrap();
    assert_eq!(config.soc, "rk3588");
    assert_eq!(config.hawkbit.poll_interval, Duration::from_secs(90));
    assert_eq!(config.hawkbit.token, "ordinary`word");
    assert!(!config.watchdog.enabled);
    assert_eq!(config.warnings.len(), 1);
}

#[test]
fn duration_compatibility_accepts_decimal_and_compound_values() {
    let cases = [
        ("+1.25s", Some(Duration::from_millis(1250))),
        ("1m.5s", Some(Duration::from_millis(60_500))),
        ("1h2m3s4ms5µs6ns", Some(Duration::new(3723, 4_005_006))),
        ("6s1μs60ns", Some(Duration::new(6, 1060))),
        ("1.s", Some(Duration::from_secs(1))),
        ("1..s", None),
        ("9h1", None),
        ("-1s", None),
        ("18446744073709551616s", None),
    ];
    for (input, expected) in cases {
        assert_eq!(parse_duration(input), expected, "{input}");
    }
}

#[test]
fn related_settings_fall_back_independently() {
    let config = load(
        "\
[watchdog]
enabled=false
timeout=5s
feed_interval=9s
[hawkbit]
enabled=true
controller_id=keep-id
[container]
enabled=true
manifest=
[boot]
check_enabled=true
check_program=
[core]
state_dir=/var/lib/custom-core
",
    )
    .unwrap();
    assert!(!config.watchdog.enabled);
    assert_eq!(config.watchdog.timeout, WatchdogConfig::default().timeout);
    assert_eq!(
        config.watchdog.feed_interval,
        WatchdogConfig::default().feed_interval
    );
    assert!(!config.hawkbit.enabled);
    assert_eq!(config.hawkbit.controller_id, "keep-id");
    assert!(!config.container_enabled);
    assert!(config.boot_check_enabled);
    assert_eq!(config.state_dir, "/var/lib/custom-core");
    assert_eq!(config.warnings.len(), 4);
}

#[test]
fn unsafe_values_retain_defaults() {
    let cases = [
        ("core", "state_dir", "/"),
        ("core", "socket", "relative"),
        ("core", "socket", "/run//core.sock"),
        ("container", "address", "/run/../tmp/socket"),
        ("container", "manifest", "/userdata/not-the-manifest.yml"),
        (
            "container",
            "manifest",
            "/var/lib/naos/containers/nested/id/manifest.yml",
        ),
        ("boot", "check_program", "/usr/bin/../local/ready"),
        ("http", "enabled", "True"),
        ("http", "listen", "localhost:8890"),
        ("http", "listen", "0.0.0.0:8890"),
        ("http", "listen", "[::1]:0"),
        ("http", "listen", "127.0.0.1:65536"),
        ("watchdog", "device", "/dev/watchdog"),
        ("watchdog", "device", "/dev/watchdog0/other"),
        ("watchdog", "timeout", "30.5s"),
        ("watchdog", "timeout", "301s"),
        ("watchdog", "feed_interval", "0s"),
        ("hawkbit", "poll_interval", "25h"),
        (
            "hawkbit",
            "server_url",
            "https://user:password@example.test",
        ),
        ("hawkbit", "server_url", "https://example.test?token=secret"),
        ("hawkbit", "server_url", "https://example.test/#private"),
        ("hawkbit", "server_url", "https:///example.test"),
        ("hawkbit", "controller_id", "path/segment"),
        ("hawkbit", "download_path", "relative"),
        ("hawkbit", "token_type", "invalid"),
    ];
    for (section, key, value) in cases {
        let mut config = load(&format!("[{section}]\n{key}={value}\n")).unwrap();
        assert_eq!(config.warnings.len(), 1, "{section}.{key}");
        assert!(config.warnings[0].contains(&format!("{section}.{key}")));
        config.warnings.clear();
        assert_eq!(config, Config::default());
    }
}

#[test]
fn rejects_oversize_files_symlinks_and_directories() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("device.ini");
    fs::write(&path, vec![b'#'; 65537]).unwrap();
    assert!(
        Config::load(&path)
            .unwrap_err()
            .to_string()
            .contains("64 KiB")
    );
    fs::write(&path, "[platform]\nsoc=rk3588").unwrap();
    let link = directory.path().join("link.ini");
    symlink(&path, &link).unwrap();
    assert!(Config::load(&link).is_err());
    assert!(save_hawkbit_registration(&link, &profile()).is_err());
    assert!(Config::load(directory.path()).is_err());
    assert_eq!(Config::load(&path).unwrap().soc, "rk3588");
}

#[test]
fn registration_retains_settings_and_comments_with_private_atomic_file() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path();
    fs::write(
        path,
        "\
# original configuration
[hawkbit] # first
enabled=false
token=old-one # credential comment
controller_id=separate-id
[platform]
soc=qcs9075
[hawkbit] ; last
token =old-token-two # second credential comment
controller_id=old-two
poll_interval=7m
[watchdog]
enabled=false
",
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    save_hawkbit_registration(path, &profile()).unwrap();
    let config = Config::load(path).unwrap();
    assert_eq!(config.soc, "qcs9075");
    assert!(!config.watchdog.enabled);
    assert!(config.hawkbit.enabled);
    assert_eq!(config.hawkbit.token, profile().token);
    assert_eq!(config.hawkbit.server_url, "https://updates.example.test");
    assert_eq!(config.hawkbit.controller_id, profile().controller_id);
    assert_eq!(config.hawkbit.token_type, "gateway");
    assert_eq!(config.hawkbit.poll_interval, Duration::from_secs(420));
    assert!(config.warnings.is_empty(), "{:?}", config.warnings);
    let raw = fs::read_to_string(path).unwrap();
    for preserved in [
        "original configuration",
        "credential comment",
        "second credential comment",
        "] ; last",
    ] {
        assert!(raw.contains(preserved));
    }
    for removed in ["old-one", "old-two", "old-token-two", "separate-id"] {
        assert!(!raw.contains(removed));
    }
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn invalid_or_oversize_registration_leaves_original_intact() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let path = file.path();
    let original = "[hawkbit]\nenabled=false\ntoken=retained-token\n";
    fs::write(path, original).unwrap();
    let mut invalid = profile();
    invalid.token = "invalid-secret # injected".into();
    let error = save_hawkbit_registration(path, &invalid).unwrap_err();
    assert!(!format!("{error:#}").contains("invalid-secret"));
    assert_eq!(fs::read_to_string(path).unwrap(), original);
    let large = format!("#{padding}\n", padding = "a".repeat(65_530));
    fs::write(path, &large).unwrap();
    assert!(save_hawkbit_registration(path, &profile()).is_err());
    assert_eq!(fs::read_to_string(path).unwrap(), large);
}

#[test]
fn new_registration_keeps_literal_quoted_credentials() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("new").join("device.ini");
    let mut profile = profile();
    for token in [
        "\"quoted\"",
        "`quoted`",
        ";no-comment",
        "#literal",
        "'single'",
    ] {
        profile.token = token.into();
        save_hawkbit_registration(&path, &profile).unwrap();
        assert_eq!(Config::load(&path).unwrap().hawkbit.token, token);
    }
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
}

#[test]
fn controller_id_generation_is_atomic_and_observes_a_concurrent_save() {
    let directory = tempfile::tempdir().unwrap();
    let path = &directory.path().join("device.ini");
    let generated = &AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let threads: Vec<_> = (0..12)
            .map(|number| {
                scope.spawn(move || {
                    ensure_controller_id(path, || {
                        generated.fetch_add(1, Ordering::SeqCst);
                        Ok(format!("NN{number}"))
                    })
                    .unwrap()
                })
            })
            .collect();
        let identities: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(identities.iter().all(|value| value == &identities[0]));
    });
    assert_eq!(generated.load(Ordering::SeqCst), 1);
    save_hawkbit_registration(path, &profile()).unwrap();
    assert_eq!(
        ensure_controller_id(path, || panic!("existing id should win")).unwrap(),
        profile().controller_id
    );
}

#[test]
fn concurrent_registrations_commit_whole_profiles() {
    let directory = tempfile::tempdir().unwrap();
    let path = &directory.path().join("device.ini");
    std::thread::scope(|scope| {
        for number in 0..8 {
            scope.spawn(move || {
                let config = HawkbitConfig {
                    tenant: format!("tenant{number}"),
                    token: format!("token{number}"),
                    controller_id: format!("controller{number}"),
                    ..profile()
                };
                save_hawkbit_registration(path, &config).unwrap();
            });
        }
    });
    let saved = Config::load(path).unwrap().hawkbit;
    let number = saved.tenant.strip_prefix("tenant").unwrap();
    assert_eq!(saved.token, format!("token{number}"));
    assert_eq!(saved.controller_id, format!("controller{number}"));
}
