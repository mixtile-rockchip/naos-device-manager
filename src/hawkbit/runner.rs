use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use chrono::{DateTime, Utc};
use serde::{Serialize, Serializer};
use tokio::sync::watch;
use url::Url;

use crate::config::HawkbitConfig;

use super::attributes::read_system_attributes;
use super::client::{
    client, duration_text, poll, report_attributes, run_with_runtime, updater_ini, validate,
};
use super::process::{
    Options, ProcessIdentity, atomic_write, has_running_executable, open_lock, open_log,
    private_dir, process_identity, random_request_id, recent_logs, rotate_log, updater_command,
};

const STOPPED_BEFORE_LAUNCH: &str = "Core stopped before launching updater";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExternalRequest {
    pub request_id: String,
    pub path: String,
    pub action_id: String,
    pub version: String,
}

pub trait UpdateHooks: Send + Sync {
    fn begin_external(&self, request: ExternalRequest) -> Result<String>;
    fn attach_external(&self, id: &str, identity: ProcessIdentity) -> Result<()>;
    fn end_external(&self, id: &str, exit_code: i32, error: Option<&str>) -> Result<()>;
}

fn serialize_timestamp<S: Serializer>(
    timestamp: &Option<DateTime<Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match timestamp {
        Some(timestamp) => timestamp.serialize(serializer),
        None => serializer.serialize_str("0001-01-01T00:00:00Z"),
    }
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct HeartbeatSnapshot {
    #[serde(serialize_with = "serialize_timestamp")]
    pub last_success: Option<DateTime<Utc>>,
    #[serde(serialize_with = "serialize_timestamp")]
    pub next_poll: Option<DateTime<Utc>>,
    pub http_status: u16,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub polling_sleep: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub action: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UpdaterSnapshot {
    pub phase: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(serialize_with = "serialize_timestamp")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(serialize_with = "serialize_timestamp")]
    pub completed_at: Option<DateTime<Utc>>,
    #[serde(serialize_with = "serialize_timestamp")]
    pub next_run: Option<DateTime<Utc>>,
    pub exit_code: Option<i32>,
    pub download_percent: Option<u8>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub error: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    pub enabled: bool,
    pub state: String,
    pub heartbeat: HeartbeatSnapshot,
    pub updater: UpdaterSnapshot,
    pub recent_logs: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub system_version: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub attributes_error: String,
}

struct Profile {
    cfg: HawkbitConfig,
    origin: Option<Url>,
    snapshot: Mutex<Snapshot>,
    cancellation: watch::Sender<bool>,
}

impl Profile {
    fn new(cfg: HawkbitConfig) -> Result<Self> {
        let mut snapshot = Snapshot {
            enabled: cfg.enabled,
            ..Snapshot::default()
        };
        let origin = if cfg.enabled {
            snapshot.state = "stopped".into();
            snapshot.updater.phase = "idle".into();
            Some(validate(
                &cfg,
                cfg.poll_interval,
                cfg.timeout,
                &cfg.download_path,
            )?)
        } else {
            snapshot.state = "disabled".into();
            snapshot.updater.phase = "disabled".into();
            None
        };
        let (cancellation, _) = watch::channel(false);
        Ok(Self {
            cfg,
            origin,
            snapshot: Mutex::new(snapshot),
            cancellation,
        })
    }

    fn cancel(&self) {
        self.cancellation.send_replace(true);
    }

    fn change(&self, change: impl FnOnce(&mut Snapshot)) {
        change(
            &mut self
                .snapshot
                .lock()
                .expect("hawkBit snapshot lock poisoned"),
        );
    }
}

struct ManagedState {
    current: Arc<Profile>,
    pending: Option<Arc<Profile>>,
    active: bool,
    started: bool,
    running: bool,
}

struct Shared {
    state: Mutex<ManagedState>,
    state_changed: Condvar,
    log_io: Mutex<()>,
    hooks: Arc<dyn UpdateHooks>,
    options: Options,
}

impl Shared {
    fn is_current(state: &ManagedState, profile: &Arc<Profile>) -> bool {
        state.running && Arc::ptr_eq(&state.current, profile)
    }

    // Claims and reloads share the same lock. A reload cannot race a queued
    // scheduler pass into starting under the profile it just replaced.
    fn claim(&self, profile: &Arc<Profile>) -> bool {
        let mut state = self.state.lock().expect("hawkBit manager lock poisoned");
        if !Self::is_current(&state, profile) {
            return false;
        }
        state.active = true;
        true
    }

    fn finish(&self, profile: &Arc<Profile>) {
        let mut state = self.state.lock().expect("hawkBit manager lock poisoned");
        state.active = false;
        if Self::is_current(&state, profile)
            && let Some(next) = state.pending.take()
        {
            state.current.cancel();
            state.current = next;
            self.state_changed.notify_all();
        }
    }

    fn wait_current(&self, profile: &Arc<Profile>, delay: Duration) -> bool {
        let state = self.state.lock().expect("hawkBit manager lock poisoned");
        let (state, _) = self
            .state_changed
            .wait_timeout_while(state, delay, |state| Self::is_current(state, profile))
            .expect("hawkBit manager lock poisoned");
        Self::is_current(&state, profile)
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct Reload {
    pub applied: bool,
    pub pending: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ReloadError {
    #[error("hawkBit configuration change is pending")]
    Busy,
    #[error("hawkBit manager is not running")]
    NotRunning,
    #[error("{0}")]
    Invalid(String),
}

pub struct Manager {
    shared: Arc<Shared>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Manager {
    pub fn new(cfg: HawkbitConfig, hooks: Arc<dyn UpdateHooks>) -> Result<Self> {
        Self::with_options(cfg, hooks, Options::default())
    }

    fn with_options(
        cfg: HawkbitConfig,
        hooks: Arc<dyn UpdateHooks>,
        options: Options,
    ) -> Result<Self> {
        let current = Arc::new(Profile::new(cfg)?);
        Ok(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(ManagedState {
                    current,
                    pending: None,
                    active: false,
                    started: false,
                    running: false,
                }),
                state_changed: Condvar::new(),
                log_io: Mutex::new(()),
                hooks,
                options,
            }),
            thread: Mutex::new(None),
        })
    }

    pub fn start(&self) -> Result<()> {
        let mut worker = self.thread.lock().expect("hawkBit worker lock poisoned");
        let mut state = self
            .shared
            .state
            .lock()
            .expect("hawkBit manager lock poisoned");
        ensure!(!state.started, "hawkBit manager has already run");
        let shared = self.shared.clone();
        *worker = Some(
            thread::Builder::new()
                .name("hawkbit-scheduler".into())
                .spawn(move || supervise(&shared))?,
        );
        state.started = true;
        state.running = true;
        Ok(())
    }

    pub fn request_stop(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("hawkBit manager lock poisoned");
        if state.running {
            state.running = false;
            state.pending = None;
            state.current.cancel();
            self.shared.state_changed.notify_all();
        }
    }

    pub fn stop(&self) -> Result<()> {
        let mut worker = self.thread.lock().expect("hawkBit worker lock poisoned");
        self.request_stop();
        let Some(worker) = worker.take() else {
            return Ok(());
        };
        worker
            .join()
            .map_err(|_| anyhow!("hawkBit manager worker stopped unexpectedly"))
    }

    pub fn reload(&self, cfg: HawkbitConfig) -> Result<Reload, ReloadError> {
        let next =
            Arc::new(Profile::new(cfg).map_err(|error| ReloadError::Invalid(error.to_string()))?);
        let mut state = self
            .shared
            .state
            .lock()
            .expect("hawkBit manager lock poisoned");
        if !state.running {
            return Err(ReloadError::NotRunning);
        }
        if state.pending.is_some() {
            return Err(ReloadError::Busy);
        }
        if state.active {
            state.pending = Some(next);
            return Ok(Reload {
                applied: false,
                pending: true,
            });
        }
        state.current.cancel();
        state.current = next;
        self.shared.state_changed.notify_all();
        Ok(Reload {
            applied: true,
            pending: false,
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        let profile = self
            .shared
            .state
            .lock()
            .expect("hawkBit manager lock poisoned")
            .current
            .clone();
        let mut snapshot = profile
            .snapshot
            .lock()
            .expect("hawkBit snapshot lock poisoned")
            .clone();
        if snapshot.enabled {
            // The log and the preceding run's INI must be read as a pair.
            let _log = self
                .shared
                .log_io
                .lock()
                .expect("hawkBit log lock poisoned");
            snapshot.recent_logs = recent_logs(&self.shared.options, &profile.cfg.token);
        }
        snapshot
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn supervise(shared: &Shared) {
    loop {
        let profile = {
            let state = shared.state.lock().expect("hawkBit manager lock poisoned");
            if !state.running {
                return;
            }
            state.current.clone()
        };
        if let Some(origin) = &profile.origin {
            if let Err(error) = run_profile(shared, &profile, origin) {
                profile.change(|snapshot| snapshot.state = "failed".into());
                eprintln!("naos-core: hawkBit stopped: {error}");
            } else {
                profile.change(|snapshot| snapshot.state = "stopped".into());
            }
        }
        let state = shared.state.lock().expect("hawkBit manager lock poisoned");
        drop(
            shared
                .state_changed
                .wait_while(state, |state| Shared::is_current(state, &profile))
                .expect("hawkBit manager lock poisoned"),
        );
    }
}

fn run_profile(shared: &Shared, profile: &Arc<Profile>, origin: &Url) -> Result<()> {
    profile.change(|snapshot| snapshot.state = "starting".into());
    private_dir(&shared.options.directory)?;
    private_dir(Path::new(&profile.cfg.download_path))?;
    let _lock = open_lock(&shared.options.directory.join("hawkbit-controller.lock"))?;
    let heartbeat = {
        let profile = profile.clone();
        let options = shared.options.clone();
        let origin = origin.clone();
        thread::Builder::new()
            .name("hawkbit-heartbeat".into())
            .spawn(move || {
                let run = || -> Result<()> {
                    let client = client(profile.cfg.timeout)?;
                    run_with_runtime(heartbeat(&profile, &options, &client, &origin))?;
                    Ok(())
                };
                if run().is_err() {
                    profile.change(|snapshot| {
                        snapshot.heartbeat.error = "cannot start hawkBit heartbeat".into()
                    });
                }
            })?
    };
    profile.change(|snapshot| snapshot.state = "running".into());
    schedule(shared, profile, origin);
    profile.cancel();
    heartbeat
        .join()
        .map_err(|_| anyhow!("hawkBit heartbeat worker stopped unexpectedly"))
}

fn backoff(base: Duration, failures: u32) -> Duration {
    let factor = 1 << failures.saturating_sub(1).min(7);
    base.saturating_mul(factor)
        .min(Duration::from_secs(24 * 3600))
}

fn after(delay: Duration) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::from_std(delay).expect("hawkBit delay is bounded")
}

async fn until_cancelled<F>(
    cancellation: &mut watch::Receiver<bool>,
    operation: F,
) -> Option<F::Output>
where
    F: std::future::Future,
{
    tokio::select! {
        biased;
        _ = cancellation.wait_for(|stopped| *stopped) => None,
        result = operation => Some(result),
    }
}

async fn heartbeat(profile: &Profile, options: &Options, client: &reqwest::Client, origin: &Url) {
    let mut cancellation = profile.cancellation.subscribe();
    let mut delay = Duration::ZERO;
    let mut failures = 0_u32;
    let mut last_attributes = Default::default();
    while until_cancelled(&mut cancellation, tokio::time::sleep(delay))
        .await
        .is_some()
    {
        let Some(result) =
            until_cancelled(&mut cancellation, poll(client, origin, &profile.cfg)).await
        else {
            break;
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                failures = failures.saturating_add(1);
                delay = backoff(
                    profile.cfg.poll_interval.min(Duration::from_secs(30)),
                    failures,
                );
                profile.change(|snapshot| {
                    snapshot.heartbeat.http_status = error.status;
                    snapshot.heartbeat.next_poll = Some(after(delay));
                    snapshot.heartbeat.error = error.message;
                });
                continue;
            }
        };
        failures = 0;
        delay = result.sleep;
        profile.change(|snapshot| {
            snapshot.heartbeat.http_status = result.status;
            snapshot.heartbeat.next_poll = Some(after(delay));
            snapshot.heartbeat.last_success = Some(Utc::now());
            snapshot.heartbeat.polling_sleep = duration_text(result.sleep);
            snapshot.heartbeat.action = result.action.into();
            snapshot.heartbeat.error.clear();
        });
        let attributes_error =
            match read_system_attributes(&options.version_file, &options.os_release) {
                Ok(attributes) => {
                    profile.change(|snapshot| {
                        snapshot.system_version = attributes
                            .get("system_version")
                            .cloned()
                            .unwrap_or_default();
                    });
                    if attributes != last_attributes || result.config_data {
                        let Some(report) = until_cancelled(
                            &mut cancellation,
                            report_attributes(client, origin, &profile.cfg, &attributes),
                        )
                        .await
                        else {
                            break;
                        };
                        match report {
                            Ok(()) => {
                                last_attributes = attributes;
                                String::new()
                            }
                            Err(error) => error.to_string(),
                        }
                    } else {
                        String::new()
                    }
                }
                Err(error) => error.to_string(),
            };
        profile.change(|snapshot| snapshot.attributes_error = attributes_error);
    }
}

fn schedule(shared: &Shared, profile: &Arc<Profile>, origin: &Url) {
    let mut delay = Duration::ZERO;
    let mut failures = 0_u32;
    while shared.wait_current(profile, delay) {
        let interval = profile.cfg.poll_interval;
        delay = match has_running_executable(&shared.options.updater) {
            Err(_) => {
                failures = failures.saturating_add(1);
                profile.change(|snapshot| {
                    snapshot.updater.phase = "blocked".into();
                    snapshot.updater.error = "cannot inspect running updater processes".into();
                });
                backoff(interval, failures)
            }
            Ok(true) => {
                profile.change(|snapshot| {
                    snapshot.updater = UpdaterSnapshot {
                        phase: "skipped".into(),
                        error: "updater already running; skipping this interval".into(),
                        ..UpdaterSnapshot::default()
                    };
                });
                interval
            }
            Ok(false) => {
                if !shared.claim(profile) {
                    break;
                }
                let succeeded = run_once(shared, profile, origin).is_ok();
                shared.finish(profile);
                failures = if succeeded {
                    0
                } else {
                    failures.saturating_add(1)
                };
                backoff(interval, failures)
            }
        };
        profile.change(|snapshot| snapshot.updater.next_run = Some(after(delay)));
    }
}

fn finish_task(
    shared: &Shared,
    profile: &Profile,
    id: &str,
    code: i32,
    error: Option<&str>,
) -> Result<()> {
    let end = shared.hooks.end_external(id, code, error);
    profile.change(|snapshot| {
        snapshot.updater.phase = "completed".into();
        snapshot.updater.completed_at = Some(Utc::now());
        snapshot.updater.exit_code = Some(code);
        if error.is_some() {
            snapshot.updater.error = "updater process failed".into();
        }
        if end.is_err() {
            snapshot.updater.error = "updater task completion requires inspection".into();
        }
    });
    end?;
    if let Some(error) = error {
        return Err(anyhow!("{error}"));
    }
    Ok(())
}

fn run_once(shared: &Shared, profile: &Arc<Profile>, origin: &Url) -> Result<()> {
    let request = ExternalRequest {
        request_id: random_request_id()?,
        path: Path::new(&profile.cfg.download_path)
            .join("hawkbit.raucb")
            .to_string_lossy()
            .into_owned(),
        ..ExternalRequest::default()
    };
    if !shared.wait_current(profile, Duration::ZERO) {
        return Err(anyhow!(STOPPED_BEFORE_LAUNCH));
    }
    let id = match shared.hooks.begin_external(request) {
        Ok(id) if !id.is_empty() => id,
        _ => {
            profile.change(|snapshot| {
                snapshot.updater.phase = "deferred".into();
                snapshot.updater.error = "update operation is busy or unavailable".into();
            });
            return Err(anyhow!("cannot reserve updater operation"));
        }
    };
    profile.change(|snapshot| {
        snapshot.updater = UpdaterSnapshot {
            phase: "starting".into(),
            task_id: id.clone(),
            started_at: Some(Utc::now()),
            ..UpdaterSnapshot::default()
        };
    });
    if !shared.wait_current(profile, Duration::ZERO) {
        return finish_task(shared, profile, &id, -1, Some(STOPPED_BEFORE_LAUNCH));
    }

    let log_path = shared.options.log_path();
    let log = match open_log(&log_path) {
        Ok(log) => log,
        Err(_) => return finish_task(shared, profile, &id, -1, Some("cannot open updater log")),
    };
    let data =
        updater_ini(&profile.cfg, origin).expect("hawkBit profile validated before scheduling");
    let config_path = shared.options.config_path();
    let publish = {
        let _guard = shared.log_io.lock().expect("hawkBit log lock poisoned");
        rotate_log(&log, &log_path, true)
            .map_err(|_| "cannot rotate updater log")
            .and_then(|_| {
                atomic_write(&config_path, &data)
                    .map_err(|_| "cannot publish updater configuration")
            })
    };
    if let Err(error) = publish {
        return finish_task(shared, profile, &id, -1, Some(error));
    }

    // Serialize child admission with request_stop after all configuration I/O.
    let child = {
        let state = shared.state.lock().expect("hawkBit manager lock poisoned");
        Shared::is_current(&state, profile).then(|| {
            updater_command(&shared.options, &log).and_then(|mut command| Ok(command.spawn()?))
        })
    };
    let mut child = match child {
        Some(Ok(child)) => child,
        Some(Err(_)) => return finish_task(shared, profile, &id, -1, Some("cannot start updater")),
        None => return finish_task(shared, profile, &id, -1, Some(STOPPED_BEFORE_LAUNCH)),
    };
    profile.change(|snapshot| {
        snapshot.updater.phase = "running".into();
        snapshot.updater.pid = Some(child.id());
    });
    if let Ok(identity) = process_identity(child.id(), &config_path, &data)
        && shared.hooks.attach_external(&id, identity).is_err()
    {
        profile.change(|snapshot| {
            snapshot.updater.error = "cannot attach updater process to task".into()
        });
    }

    // This Core instance waits only for the child it started. A shutdown
    // detaches that child and leaves its durable task for later reconciliation.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let _guard = shared.log_io.lock().expect("hawkBit log lock poisoned");
                let _ = rotate_log(&log, &log_path, false);
                drop(_guard);
                return finish_task(
                    shared,
                    profile,
                    &id,
                    status.code().unwrap_or(-1),
                    (!status.success()).then_some("updater process failed"),
                );
            }
            Err(_) => {
                return finish_task(
                    shared,
                    profile,
                    &id,
                    -1,
                    Some("cannot read updater exit status"),
                );
            }
            Ok(None) => {}
        }
        if !shared.wait_current(profile, shared.options.monitor_interval) {
            profile.change(|snapshot| snapshot.updater.phase = "detached".into());
            let _ = thread::Builder::new()
                .name("hawkbit-updater-reaper".into())
                .spawn(move || {
                    let _ = child.wait();
                });
            return Err(anyhow!("Core stopped while updater was active"));
        }
        let _guard = shared.log_io.lock().expect("hawkBit log lock poisoned");
        if rotate_log(&log, &log_path, false).is_err() {
            profile.change(|snapshot| snapshot.updater.error = "cannot rotate updater log".into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Instant;

    #[derive(Debug)]
    enum Event {
        Started(ExternalRequest),
        Attached(i32),
        Finished(i32),
    }

    struct Hooks {
        events: Sender<Event>,
        block_begin: Mutex<Option<Receiver<()>>>,
    }

    impl UpdateHooks for Hooks {
        fn begin_external(&self, request: ExternalRequest) -> Result<String> {
            self.events.send(Event::Started(request))?;
            if let Some(block) = self.block_begin.lock().unwrap().take() {
                block.recv_timeout(Duration::from_secs(2))?;
            }
            Ok("task-001".into())
        }

        fn attach_external(&self, id: &str, identity: ProcessIdentity) -> Result<()> {
            assert_eq!(id, "task-001");
            assert!(identity.start_time > 0);
            self.events.send(Event::Attached(identity.pid))?;
            Ok(())
        }

        fn end_external(&self, id: &str, code: i32, _error: Option<&str>) -> Result<()> {
            assert_eq!(id, "task-001");
            self.events.send(Event::Finished(code))?;
            Ok(())
        }
    }

    fn profile(directory: &Path, enabled: bool) -> HawkbitConfig {
        HawkbitConfig {
            enabled,
            server_url: "http://127.0.0.1:9".into(),
            tenant: "DEFAULT".into(),
            controller_id: "device-1".into(),
            token: "synthetic-token".into(),
            token_type: "gateway".into(),
            poll_interval: Duration::from_secs(60),
            timeout: Duration::from_secs(1),
            download_path: directory.join("downloads").to_string_lossy().into_owned(),
        }
    }

    fn options(directory: &Path, commands: &str) -> Options {
        let updater = directory.join("fake-updater");
        fs::write(&updater, format!("#!/bin/sh\n{commands}\n")).unwrap();
        fs::set_permissions(&updater, fs::Permissions::from_mode(0o700)).unwrap();
        Options {
            updater,
            directory: directory.join("run"),
            version_file: directory.join("version.json"),
            os_release: directory.join("os-release"),
            monitor_interval: Duration::from_millis(5),
        }
    }

    fn hooks(block_begin: Option<Receiver<()>>) -> (Arc<Hooks>, Receiver<Event>) {
        let (events, received) = mpsc::channel();
        (
            Arc::new(Hooks {
                events,
                block_begin: Mutex::new(block_begin),
            }),
            received,
        )
    }

    fn event(events: &Receiver<Event>) -> Event {
        events.recv_timeout(Duration::from_secs(3)).unwrap()
    }

    fn wait_for(mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !predicate() {
            assert!(Instant::now() < deadline, "condition did not complete");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn backoff_is_capped_at_a_day_and_keeps_first_interval() {
        let base = Duration::from_secs(10);
        assert_eq!(backoff(base, 0), base);
        assert_eq!(backoff(base, 1), base);
        assert_eq!(backoff(base, 4), Duration::from_secs(80));
        assert_eq!(
            backoff(Duration::from_secs(7200), 20),
            Duration::from_secs(86400)
        );
    }

    #[test]
    fn disabled_startup_does_not_touch_runtime_files_and_accepts_idle_reload() {
        let folder = tempfile::tempdir().unwrap();
        let opts = options(folder.path(), "sleep 0.05; exit 0");
        let download = folder.path().join("downloads");
        let runtime = opts.directory.clone();
        let (hooks, events) = hooks(None);
        let manager = Manager::with_options(profile(folder.path(), false), hooks, opts).unwrap();
        assert!(matches!(
            manager.reload(profile(folder.path(), true)),
            Err(ReloadError::NotRunning)
        ));
        manager.start().unwrap();
        assert!(!download.exists());
        assert!(!runtime.exists());
        assert!(!manager.snapshot().enabled);
        assert!(matches!(
            manager.reload(profile(folder.path(), true)),
            Ok(Reload {
                applied: true,
                pending: false
            })
        ));
        let Event::Started(request) = event(&events) else {
            panic!("expected task start")
        };
        assert!(request.request_id.starts_with("hawkbit-"));
        assert_eq!(PathBuf::from(request.path), download.join("hawkbit.raucb"));
        assert!(runtime.exists());
        let mut next = event(&events);
        if let Event::Attached(pid) = next {
            assert!(pid > 0);
            next = event(&events);
        }
        assert!(matches!(next, Event::Finished(0)));
        wait_for(|| manager.snapshot().updater.exit_code == Some(0));
        manager.stop().unwrap();
    }

    #[test]
    fn pending_reload_waits_for_owned_updater_completion_and_rejects_second_change() {
        let folder = tempfile::tempdir().unwrap();
        let opts = options(folder.path(), "sleep 0.05; exit 7");
        let (release, blocked) = mpsc::channel();
        let (hooks, events) = hooks(Some(blocked));
        let manager = Manager::with_options(profile(folder.path(), true), hooks, opts).unwrap();
        manager.start().unwrap();
        assert!(matches!(event(&events), Event::Started(_)));
        assert_eq!(
            manager.reload(profile(folder.path(), false)).unwrap(),
            Reload {
                applied: false,
                pending: true
            }
        );
        assert!(matches!(
            manager.reload(profile(folder.path(), false)),
            Err(ReloadError::Busy)
        ));
        let mut invalid = profile(folder.path(), true);
        invalid.token = "private credential that cannot be logged".into();
        let error = manager.reload(invalid).unwrap_err();
        assert!(matches!(error, ReloadError::Invalid(_)));
        assert!(!error.to_string().contains("private credential"));
        assert!(manager.snapshot().enabled);
        release.send(()).unwrap();
        let mut next = event(&events);
        if let Event::Attached(pid) = next {
            assert!(pid > 0);
            next = event(&events);
        }
        assert!(matches!(next, Event::Finished(7)));
        wait_for(|| !manager.snapshot().enabled);
        assert_eq!(manager.snapshot().state, "disabled");
        manager.stop().unwrap();
    }

    #[test]
    fn request_stop_during_reservation_releases_task_without_launching_updater() {
        let folder = tempfile::tempdir().unwrap();
        let opts = options(folder.path(), r#": > "$0.started""#);
        let marker = opts.updater.with_extension("started");
        let (release, blocked) = mpsc::channel();
        let (hooks, events) = hooks(Some(blocked));
        let manager = Manager::with_options(profile(folder.path(), true), hooks, opts).unwrap();
        manager.start().unwrap();
        assert!(matches!(event(&events), Event::Started(_)));

        manager.request_stop();
        assert!(matches!(
            manager.reload(profile(folder.path(), false)),
            Err(ReloadError::NotRunning)
        ));
        release.send(()).unwrap();
        manager.stop().unwrap();

        assert!(matches!(event(&events), Event::Finished(-1)));
        assert!(events.try_recv().is_err());
        assert!(!marker.exists());
    }

    #[test]
    fn request_stop_before_config_publication_completes_prevents_child_admission() {
        let folder = tempfile::tempdir().unwrap();
        let opts = options(folder.path(), r#": > "$0.started""#);
        let marker = opts.updater.with_extension("started");
        let log = opts.log_path();
        let (hooks, events) = hooks(None);
        let manager = Manager::with_options(profile(folder.path(), true), hooks, opts).unwrap();
        let publication = manager.shared.log_io.lock().unwrap();
        manager.start().unwrap();
        assert!(matches!(event(&events), Event::Started(_)));
        wait_for(|| log.is_file());

        manager.request_stop();
        drop(publication);
        manager.stop().unwrap();

        assert!(matches!(event(&events), Event::Finished(-1)));
        assert!(events.try_recv().is_err());
        assert!(!marker.exists());
    }

    #[test]
    fn shutdown_detaches_a_running_child_and_does_not_invent_a_completion() {
        let folder = tempfile::tempdir().unwrap();
        let opts = options(folder.path(), "sleep 0.75");
        let (hooks, events) = hooks(None);
        let manager = Manager::with_options(profile(folder.path(), true), hooks, opts).unwrap();
        manager.start().unwrap();
        assert!(matches!(event(&events), Event::Started(_)));
        let Event::Attached(pid) = event(&events) else {
            panic!("expected running updater identity")
        };
        let proc = PathBuf::from(format!("/proc/{pid}"));
        manager.request_stop();
        manager.stop().unwrap();
        assert_eq!(manager.snapshot().updater.phase, "detached");
        assert!(proc.exists());
        assert!(events.try_recv().is_err());
        wait_for(|| !proc.exists());
    }

    #[test]
    fn startup_failure_is_visible_and_does_not_reserve_an_update() {
        let folder = tempfile::tempdir().unwrap();
        let opts = options(folder.path(), "exit 0");
        fs::create_dir(&opts.directory).unwrap();
        fs::set_permissions(&opts.directory, fs::Permissions::from_mode(0o755)).unwrap();
        let (hooks, events) = hooks(None);
        let manager = Manager::with_options(profile(folder.path(), true), hooks, opts).unwrap();
        manager.start().unwrap();
        wait_for(|| manager.snapshot().state == "failed");
        assert!(events.try_recv().is_err());
        manager.stop().unwrap();
    }

    #[test]
    fn scheduler_reservation_does_not_block_heartbeat_and_version_report() {
        let folder = tempfile::tempdir().unwrap();
        let opts = options(folder.path(), "exit 0");
        fs::write(
            &opts.version_file,
            r#"{"version":"1.2.3","machine":"fixture"}"#,
        )
        .unwrap();
        fs::write(
            &opts.os_release,
            "ID=alpine\nNAME=Alpine\nVERSION_ID=3.24\n",
        )
        .unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (reported, report) = mpsc::channel();
        let server = thread::spawn(move || {
            use std::io::ErrorKind::WouldBlock;
            for method in ["GET", "PUT"] {
                let deadline = Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == WouldBlock => {
                            assert!(Instant::now() < deadline, "mock {method} never arrived");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("mock accept: {error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(&mut stream);
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let route = if method == "GET" { "" } else { "/configData" };
                assert_eq!(
                    first,
                    format!("{method} /DEFAULT/controller/v1/device-1{route} HTTP/1.1\r\n")
                );
                let mut size = 0;
                let mut authenticated = false;
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':') {
                        if key.eq_ignore_ascii_case("content-length") {
                            size = value.trim().parse::<usize>().unwrap();
                        }
                        if key.eq_ignore_ascii_case("authorization") {
                            authenticated = value.trim() == "GatewayToken synthetic-token";
                        }
                    }
                }
                assert!(authenticated);
                let mut body = vec![0; size];
                reader.read_exact(&mut body).unwrap();
                if method == "GET" {
                    let response = r#"{"config":{"polling":{"sleep":"00:00:10"}},"_links":{"configData":{"href":"http://127.0.0.1:9/ignore"},"deploymentBase":{}}}"#;
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    reported
                        .send(serde_json::from_slice::<serde_json::Value>(&body).unwrap())
                        .unwrap();
                }
            }
        });

        let (release, blocked) = mpsc::channel();
        let (hooks, events) = hooks(Some(blocked));
        let mut cfg = profile(folder.path(), true);
        cfg.server_url = url;
        let manager = Manager::with_options(cfg, hooks, opts).unwrap();
        manager.start().unwrap();
        assert!(matches!(event(&events), Event::Started(_)));
        let reported = report.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(reported["mode"], "merge");
        assert_eq!(reported["data"]["system_version"], "1.2.3");
        assert_eq!(reported["data"]["os_version"], "3.24");
        assert_eq!(manager.snapshot().heartbeat.action, "deployment");
        assert_eq!(manager.snapshot().heartbeat.polling_sleep, "10s");
        assert_eq!(manager.snapshot().system_version, "1.2.3");
        assert!(events.try_recv().is_err());
        release.send(()).unwrap();
        server.join().unwrap();
        manager.stop().unwrap();
    }
}
