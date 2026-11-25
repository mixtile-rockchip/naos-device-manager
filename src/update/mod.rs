mod dbus;
mod evidence;
mod model;
mod process;
mod store;

#[cfg(test)]
mod tests;

pub use model::*;

use crate::operation::{Gate, Lease};
use chrono::Utc;
use evidence::{Bundle, Snapshot};
use process::{Environment, System};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, TryLockError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use store::{HISTORY, MAX_TASKS, PENDING, Store};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid update request: {0}")]
    Invalid(String),
    #[error("RAUC is busy")]
    Busy,
    #[error("update task or bundle not found")]
    NotFound,
    #[error("request_id was used for a different update")]
    Conflict,
    #[error("cannot durably save update task: {0}")]
    Persistence(String),
    #[error("{0}")]
    Unavailable(String),
}

impl Error {
    fn persistence(error: impl std::fmt::Display) -> Self {
        Self::Persistence(error.to_string())
    }

    fn unavailable(error: impl std::fmt::Display) -> Self {
        Self::Unavailable(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

const CALL_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy)]
struct Deadline(Instant);

impl Deadline {
    fn new() -> Self {
        Self(Instant::now() + CALL_TIMEOUT)
    }

    fn remaining(self) -> Result<Duration> {
        self.0
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(Self::expired)
    }

    fn expired() -> Error {
        Error::Unavailable("RAUC request timed out".into())
    }
}

enum InstallError {
    Rejected(Error),
    Uncertain(Error),
}

trait Backend: Send {
    fn status(&mut self, deadline: Deadline) -> Result<Status>;
    fn snapshot(&mut self, deadline: Deadline) -> Result<Snapshot>;
    fn inspect(&mut self, path: &str, deadline: Deadline) -> Result<Bundle>;
    fn install(
        &mut self,
        path: &str,
        id: &str,
        manifest: &str,
        deadline: Deadline,
    ) -> std::result::Result<(), InstallError>;
    fn watch_external(&mut self, path: &str, deadline: Deadline) -> Result<()>;
    fn release(&mut self);
}

pub struct Client {
    state: Arc<Mutex<State>>,
    observer: Option<Observer>,
}

struct Observer {
    wake: mpsc::SyncSender<()>,
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
}

struct State {
    store: Store,
    history: Vec<Task>,
    external: Option<Task>,
    last_empty: Option<Task>,
    gate: Gate,
    lease: Option<Lease>,
    active: Option<String>,
    boot_id: String,
    backend: Box<dyn Backend>,
    environment: Box<dyn Environment>,
    last_progress: Instant,
    unavailable_since: Option<Instant>,
    persistence_error: Option<String>,
}

impl Client {
    pub fn open(dir: impl AsRef<Path>, gate: Gate) -> Result<Self> {
        let boot = process::boot_id()?;
        let backend = dbus::BackendClient::spawn()?;
        Self::with_backend(
            dir.as_ref(),
            gate,
            boot,
            Box::new(backend),
            Box::new(System),
            true,
        )
    }

    fn with_backend(
        dir: &Path,
        gate: Gate,
        boot_id: String,
        backend: Box<dyn Backend>,
        environment: Box<dyn Environment>,
        start_observer: bool,
    ) -> Result<Self> {
        let store = Store::open(dir)?;
        let history = store.read(HISTORY, MAX_TASKS)?.unwrap_or_default();
        let mut state = State {
            store,
            history,
            external: None,
            last_empty: None,
            gate,
            lease: None,
            active: None,
            boot_id,
            backend,
            environment,
            last_progress: Instant::now(),
            unavailable_since: None,
            persistence_error: None,
        };
        state.restore_pending()?;
        let recovery = state.external.as_ref().or_else(|| {
            state.history.last().filter(|t| {
                staging(t)
                    || t.state == UNKNOWN && !resolved_boot_outcome(t)
                    || state.external_may_run(t)
            })
        });
        if let Some(id) = recovery.map(|task| task.id.clone()) {
            state.acquire("update-recovery")?;
            state.active = Some(id);
        }
        let state = Arc::new(Mutex::new(state));
        let observer = if start_observer {
            Some(Observer::spawn(Arc::clone(&state))?)
        } else {
            None
        };
        Ok(Self { state, observer })
    }

    pub fn status(&self) -> Result<Status> {
        let deadline = Deadline::new();
        let mut state = self.lock_until(deadline)?;
        let mut status = state.backend.status(deadline)?;
        status.pending |= state.active.is_some() || state.persistence_error.is_some();
        Ok(status)
    }

    pub fn busy(&self) -> Result<bool> {
        self.status()
            .map(|status| status.pending || status.operation != "idle")
    }

    pub fn install_request(&self, path: &str, request_id: &str) -> Result<Task> {
        self.install_until(path, request_id, Deadline::new())
    }

    fn install_until(&self, path: &str, request_id: &str, deadline: Deadline) -> Result<Task> {
        let path = validate_request(path, request_id)?;
        let mut state = self.lock_until(deadline)?;
        if let Some(task) = state.prior(request_id, &path, "local")? {
            return Ok(task);
        }
        state.acquire("local-update")?;
        let result = state.install_local(&path, request_id, deadline);
        if state.active.is_none() {
            state.release();
        } else {
            self.notify();
        }
        result
    }

    pub fn tasks(&self) -> Result<Vec<Task>> {
        let state = self.lock()?;
        state.check_persistence()?;
        let mut tasks = Vec::with_capacity(state.history.len() + 1);
        if let Some(external) = state.external.as_ref()
            && !state.history.iter().any(|task| task.id == external.id)
        {
            tasks.push(external.clone());
        }
        tasks.extend(state.history.iter().rev().cloned());
        Ok(tasks)
    }

    pub fn task(&self, id: &str) -> Result<Task> {
        let state = self.lock()?;
        let task = state.find(id).ok_or(Error::NotFound)?;
        state.check_persistence()?;
        Ok(task.clone())
    }

    pub fn begin_external(&self, request: ExternalRequest) -> Result<Task> {
        self.begin_external_until(request, Deadline::new())
    }

    fn begin_external_until(
        &self,
        mut request: ExternalRequest,
        deadline: Deadline,
    ) -> Result<Task> {
        if request.source.is_empty() {
            request.source = "hawkbit".into();
        }
        if request.source != "hawkbit"
            || request.request_id.is_empty()
            || request.version.len() > 256
            || request.action_id.len() > 128
        {
            return Err(Error::Invalid("invalid external update metadata".into()));
        }
        request.path = validate_request(&request.path, &request.request_id)?;
        let mut state = self.lock_until(deadline)?;
        let result = state.begin_external(request, deadline);
        if state.active.is_none() {
            state.release();
        } else {
            self.notify();
        }
        result
    }

    pub fn attach_external(&self, id: &str, mut identity: ExternalIdentity) -> Result<()> {
        let mut state = self.lock()?;
        let mut task = state
            .find(id)
            .filter(|t| t.source == "hawkbit")
            .cloned()
            .ok_or(Error::NotFound)?;
        if state.active.as_deref() != Some(id)
            || identity.pid <= 0
            || identity.start_time == 0
            || identity.boot_id != state.boot_id
            || !Path::new(&identity.exe).is_absolute()
            || !Path::new(&identity.config_path).is_absolute()
            || !evidence::checksum(&identity.config_sha256)
            || !state.environment.process_state(&identity, &state.boot_id).0
        {
            return Err(Error::Invalid("invalid updater process identity".into()));
        }
        let digest = process::config_digest(&identity.config_path)?;
        if !digest.eq_ignore_ascii_case(&identity.config_sha256) {
            return Err(Error::Invalid(
                "updater configuration digest differs".into(),
            ));
        }
        identity.config_sha256 = digest;
        task.external_identity = Some(identity);
        state.commit(task)?;
        Ok(())
    }

    pub fn end_external(&self, id: &str, code: i32, run_error: Option<&str>) -> Result<Task> {
        let deadline = Deadline::new();
        // The exit must be recorded even if a previous observer delays this
        // lock and leaves no RAUC observation time.
        let mut state = self.lock()?;
        if let Some(recent) = state.last_empty.as_ref().filter(|task| task.id == id) {
            return Ok(recent.clone());
        }
        let mut task = state
            .find(id)
            .filter(|t| t.source == "hawkbit")
            .cloned()
            .ok_or(Error::NotFound)?;
        task.updater_exit = Some(code);
        if terminal(&task) {
            let task = state.commit(task)?;
            if state.active.as_deref() == Some(id) {
                state.release();
            }
            return Ok(task);
        }
        if run_error.is_some() {
            task.error = "updater process failed".into();
        }
        task = state.commit(task)?;
        let result = state
            .backend
            .status(deadline)
            .and_then(|status| state.reconcile(status, deadline));
        match result {
            Err(error @ Error::Persistence(_)) => Err(error),
            Err(error) => {
                task.state = UNKNOWN.into();
                task.error = "updater exited; RAUC evidence unavailable".into();
                state.commit(task)?;
                state.release();
                Err(error)
            }
            Ok(()) => state
                .last_empty
                .as_ref()
                .filter(|task| task.id == id)
                .or_else(|| state.find(id))
                .cloned()
                .ok_or(Error::NotFound),
        }
    }

    pub fn prepare_auto_reboot(&self, attempted_id: &str) -> Result<Option<(Task, Lease)>> {
        self.prepare_auto_reboot_until(attempted_id, Deadline::new())
    }

    fn prepare_auto_reboot_until(
        &self,
        attempted_id: &str,
        deadline: Deadline,
    ) -> Result<Option<(Task, Lease)>> {
        let mut state = self.lock_until(deadline)?;
        let Some(task) = state.history.last().cloned() else {
            return Ok(None);
        };
        if task.id == attempted_id {
            return Ok(None);
        }
        state.check_persistence()?;
        if state.active.is_some()
            || task.state != INSTALLED_PENDING_REBOOT
            || task.boot_id != state.boot_id
            || task.result.is_some_and(|r| r != 0)
            || task.source == "hawkbit" && task.updater_exit != Some(0)
            || !evidence::verified_reboot_task(&task)
        {
            return Ok(None);
        }
        let Ok(lease) = state.gate.try_acquire("update-auto-reboot") else {
            return Ok(None);
        };
        let status = state.backend.status(deadline)?;
        if status.operation != "idle"
            || status.pending
            || !evidence::ready_for_reboot(&task, &state.backend.snapshot(deadline)?)
            || state.environment.updater_present() != (false, true)
        {
            return Ok(None);
        }
        deadline.remaining()?;
        Ok(Some((task, lease)))
    }

    fn notify(&self) {
        if let Some(observer) = &self.observer {
            let _ = observer.wake.try_send(());
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, State>> {
        self.state
            .lock()
            .map_err(|_| Error::Unavailable("update state lock is unavailable".into()))
    }

    fn lock_until(&self, deadline: Deadline) -> Result<MutexGuard<'_, State>> {
        loop {
            let remaining = deadline.remaining()?;
            match self.state.try_lock() {
                Ok(state) => {
                    deadline.remaining()?;
                    return Ok(state);
                }
                Err(TryLockError::Poisoned(_)) => {
                    return Err(Error::Unavailable(
                        "update state lock is unavailable".into(),
                    ));
                }
                Err(TryLockError::WouldBlock) => {
                    thread::sleep(remaining.min(Duration::from_millis(5)));
                }
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Some(observer) = self.observer.take() {
            observer.stop.store(true, Ordering::Release);
            let _ = observer.wake.try_send(());
            let _ = observer.thread.join();
        }
    }
}

impl Observer {
    fn spawn(state: Arc<Mutex<State>>) -> Result<Self> {
        let (wake, receiver) = mpsc::sync_channel(1);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("naos-update-observer".into())
            .spawn(move || {
                while !stopped.load(Ordering::Acquire) {
                    let _ = receiver.recv_timeout(Duration::from_secs(1));
                    if stopped.load(Ordering::Acquire) {
                        return;
                    }
                    let Ok(mut state) = state.lock() else { return };
                    state.observe();
                }
            })
            .map_err(Error::unavailable)?;
        Ok(Self { wake, stop, thread })
    }
}

impl State {
    fn check_persistence(&self) -> Result<()> {
        self.persistence_error
            .clone()
            .map_or(Ok(()), |msg| Err(Error::Persistence(msg)))
    }

    fn set_persistence(&mut self, result: Result<()>) -> Result<()> {
        match &result {
            Ok(()) => self.persistence_error = None,
            Err(Error::Persistence(msg)) => self.persistence_error = Some(msg.clone()),
            Err(error) => self.persistence_error = Some(error.to_string()),
        }
        result
    }

    fn acquire(&mut self, kind: &str) -> Result<()> {
        self.check_persistence()?;
        if self.active.is_some() {
            return Err(Error::Busy);
        }
        self.lease = Some(self.gate.try_acquire(kind).map_err(|_| Error::Busy)?);
        Ok(())
    }

    fn release(&mut self) {
        self.lease = None;
        self.active = None;
        self.backend.release();
    }

    fn find(&self, id: &str) -> Option<&Task> {
        self.external
            .as_ref()
            .filter(|task| task.id == id)
            .or_else(|| self.history.iter().find(|task| task.id == id))
    }

    fn prior(&self, id: &str, path: &str, source: &str) -> Result<Option<Task>> {
        if id.is_empty() {
            return Ok(None);
        }
        for task in self
            .history
            .iter()
            .chain(self.external.iter())
            .chain(self.last_empty.iter())
        {
            if task.request_id == id {
                if task.path != path || task.source != source {
                    return Err(Error::Conflict);
                }
                self.check_persistence()?;
                return Ok(Some(task.clone()));
            }
        }
        Ok(None)
    }

    fn prepare(
        &mut self,
        source: &str,
        path: &str,
        id: &str,
        version: &str,
        action: &str,
        deadline: Deadline,
    ) -> Result<Task> {
        let status = self.backend.status(deadline)?;
        if status.operation != "idle" || status.pending {
            return Err(Error::Busy);
        }
        let live = self.backend.snapshot(deadline)?;
        let slot = evidence::target(&live)?;
        let now = Utc::now();
        Ok(Task {
            id: task_id()?,
            request_id: id.into(),
            source: source.into(),
            path: path.into(),
            action_id: action.into(),
            state: SUBMITTED.into(),
            created_at: now,
            updated_at: now,
            version: version.into(),
            boot_id: self.boot_id.clone(),
            target_slot: slot.name.clone(),
            target_bootname: slot.bootname.clone(),
            boot_slot: live.boot_slot,
            primary: live.primary,
            before: live.slots,
            ..Task::default()
        })
    }

    fn install_local(&mut self, path: &str, request_id: &str, deadline: Deadline) -> Result<Task> {
        match fs::symlink_metadata(path) {
            Ok(file) if !file.is_file() => {
                return Err(Error::Invalid("update bundle is not a regular file".into()));
            }
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(Error::NotFound),
            Err(err) => return Err(Error::unavailable(err)),
            _ => {}
        }
        let mut task = self.prepare("local", path, request_id, "", "", deadline)?;
        let bundle = evidence::validate_bundle(self.backend.inspect(path, deadline)?)?;
        task.version = bundle.version;
        task.manifest_hash = bundle.hash;
        task.images = bundle.images;
        if evidence::bootloader_task(&task) {
            let original = evidence::actual_root(&task.before, &task.boot_slot)
                .filter(|slot| slot.boot_status == "good" && slot.name == task.primary)
                .filter(|_| evidence::check_topology(&task.before))
                .ok_or_else(|| {
                    Error::Unavailable(
                        "bootloader update requires a confirmed running primary OS slot".into(),
                    )
                })?;
            if evidence::loader_pair(&task.before).is_none() {
                return Err(Error::Unavailable(
                    "bootloader update requires the shared uboot and idb slots".into(),
                ));
            }
            task.target_slot = original.name.clone();
            task.target_bootname = original.bootname.clone();
        }
        // The call is never retried after uncertain acceptance or restart.
        deadline.remaining()?;
        task = self.commit(task)?;
        self.active = Some(task.id.clone());
        self.last_progress = Instant::now();
        // If the journal sync used the remaining budget, no call has been sent.
        let submission = match deadline.remaining() {
            Ok(_) => self
                .backend
                .install(path, &task.id, &task.manifest_hash, deadline),
            Err(error) => Err(InstallError::Rejected(error)),
        };
        match submission {
            Ok(()) => {
                task.state = INSTALLING.into();
                self.commit(task)
            }
            Err(InstallError::Rejected(error)) => {
                task.state = FAILED.into();
                task.error = error.to_string();
                self.commit(task)?;
                self.release();
                Err(error)
            }
            Err(InstallError::Uncertain(error)) => {
                task.error = format!("RAUC acceptance is uncertain: {error}");
                self.commit(task)?;
                Err(error)
            }
        }
    }

    fn begin_external(&mut self, request: ExternalRequest, deadline: Deadline) -> Result<Task> {
        if let Some(mut task) = self.prior(&request.request_id, &request.path, &request.source)? {
            if terminal(&task) {
                return Ok(task);
            }
            if self.active.as_ref().is_some_and(|id| id != &task.id) {
                return Err(Error::Busy);
            }
            if self.lease.is_none() {
                self.acquire("hawkbit-update")?;
            }
            task.state = RUNNING_EXTERNAL.into();
            let needs_subscription = self.active.is_none();
            deadline.remaining()?;
            if needs_subscription {
                self.backend.watch_external(&task.path, deadline)?;
            }
            deadline.remaining()?;
            let task = self.commit(task)?;
            self.active = Some(task.id.clone());
            self.last_progress = Instant::now();
            deadline.remaining()?;
            return Ok(task);
        }
        self.acquire("hawkbit-update")?;
        let mut task = self.prepare(
            &request.source,
            &request.path,
            &request.request_id,
            &request.version,
            &request.action_id,
            deadline,
        )?;
        task.state = RUNNING_EXTERNAL.into();
        self.backend.watch_external(&task.path, deadline)?;
        deadline.remaining()?;
        self.active = Some(task.id.clone());
        self.last_progress = Instant::now();
        let task = self.commit(task)?;
        deadline.remaining()?;
        Ok(task)
    }

    fn restore_pending(&mut self) -> Result<()> {
        let Some(mut records) = self.store.read(PENDING, 1)? else {
            return Ok(());
        };
        let task = records.pop().ok_or_else(|| {
            Error::Persistence("pending updater journal must contain exactly one task".into())
        })?;
        if task.source != "hawkbit" {
            return Err(Error::Persistence(
                "pending updater journal has an invalid source".into(),
            ));
        }
        for old in &self.history {
            if old.id == task.id {
                if old.source != "hawkbit"
                    || old.request_id != task.request_id
                    || old.path != task.path
                {
                    return Err(Error::Persistence(
                        "pending updater conflicts with durable task".into(),
                    ));
                }
                return self.store.clear_pending();
            }
            if !task.request_id.is_empty() && old.request_id == task.request_id {
                return Err(Error::Persistence(
                    "pending updater request conflicts with task history".into(),
                ));
            }
        }
        self.external = Some(task.clone());
        if !staging(&task) {
            self.commit(task)?;
        }
        Ok(())
    }

    fn commit(&mut self, task: Task) -> Result<Task> {
        let sidecar = self.external.as_ref().is_some_and(|t| t.id == task.id)
            || task.source == "hawkbit"
                && task.state == RUNNING_EXTERNAL
                && !self.history.iter().any(|t| t.id == task.id);
        if sidecar {
            self.persist_external(task)
        } else {
            self.commit_history(task)
        }
    }

    fn commit_history(&mut self, mut task: Task) -> Result<Task> {
        let index = self.history.iter().position(|old| old.id == task.id);
        if let Some(index) = index {
            if self.history[index] == task {
                return Ok(task);
            }
            task.updated_at = Utc::now();
        }
        let mut next = self.history.clone();
        if let Some(index) = index {
            next[index] = task.clone();
        } else {
            next.push(task.clone());
        }
        if next.len() > MAX_TASKS {
            next.remove(0);
        }
        self.set_persistence(self.store.save(HISTORY, &next, MAX_TASKS))?;
        self.history = next;
        Ok(task)
    }

    fn persist_external(&mut self, mut task: Task) -> Result<Task> {
        if let Some(old) = &self.external {
            if old == &task && self.persistence_error.is_none() && staging(&task) {
                return Ok(task);
            }
            if old != &task {
                task.updated_at = Utc::now();
            }
        }
        // Keep the desired sidecar available when an fsync fails. Admission
        // stays reserved until the observer completes the durability boundary.
        self.external = Some(task.clone());
        self.set_persistence(self.store.save(PENDING, std::slice::from_ref(&task), 1))?;
        if !staging(&task) {
            task = self.commit_history(task)?;
            self.set_persistence(self.store.clear_pending())?;
            self.external = None;
        }
        Ok(task)
    }

    fn retry_persistence(&mut self) -> Result<()> {
        if self.persistence_error.is_none() {
            return Ok(());
        }
        if let Some(task) = self.external.clone() {
            self.persist_external(task)?;
        } else {
            self.set_persistence(self.store.save(HISTORY, &self.history, MAX_TASKS))?;
        }
        Ok(())
    }

    fn discard_empty(&mut self, mut task: Task) -> Result<()> {
        self.set_persistence(self.store.clear_pending())?;
        task.state = UNKNOWN.into();
        task.error = EMPTY_CHECK.into();
        self.last_empty = Some(task);
        self.external = None;
        self.release();
        Ok(())
    }

    fn external_may_run(&self, task: &Task) -> bool {
        if task.source != "hawkbit" || task.updater_exit.is_some() || task.boot_id != self.boot_id {
            return false;
        }
        match &task.external_identity {
            Some(identity) => {
                self.environment.process_state(identity, &self.boot_id) != (false, true)
            }
            None => self.environment.updater_present() != (false, true),
        }
    }

    fn observe(&mut self) {
        if self.retry_persistence().is_err() {
            return;
        }
        if self.active.is_none()
            && !self
                .history
                .iter()
                .any(|t| t.state == INSTALLED_PENDING_REBOOT && t.boot_id != self.boot_id)
        {
            return;
        }
        let deadline = Deadline::new();
        match self
            .backend
            .status(deadline)
            .and_then(|status| self.reconcile(status, deadline))
        {
            Ok(()) => self.unavailable_since = None,
            Err(Error::Persistence(_)) => {}
            Err(_) => {
                let since = self.unavailable_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(30) {
                    self.observation_unavailable();
                }
            }
        }
    }

    fn observation_unavailable(&mut self) {
        let Some(mut task) = self.active.as_deref().and_then(|id| self.find(id)).cloned() else {
            return;
        };
        let alive = self.external_may_run(&task);
        task.state = UNKNOWN.into();
        task.error = "RAUC observation unavailable; installation result is unknown".into();
        if self.commit(task).is_ok() && !alive {
            self.release();
        }
    }

    fn reconcile(&mut self, status: Status, deadline: Deadline) -> Result<()> {
        self.retry_persistence()?;
        if let Some(id) = self.active.clone() {
            let mut task = self
                .find(&id)
                .cloned()
                .ok_or_else(|| Error::Unavailable("active update task missing".into()))?;
            task.progress = status.progress;
            if status.operation != "idle" {
                task.observed_installing = true;
                if task.source == "local" {
                    task.state = INSTALLING.into();
                }
                self.last_progress = Instant::now();
                return self.commit(task).map(drop);
            }
            let live = self.backend.snapshot(deadline)?;
            if status.result.is_some() {
                task.result = status.result;
            }
            let alive = self.external_may_run(&task);
            if task.result.is_some_and(|code| code != 0) {
                task.state = FAILED.into();
                task.error = if status.last_error.is_empty() {
                    "RAUC installation failed".into()
                } else {
                    status.last_error
                };
                self.commit(task)?;
                if !alive {
                    self.release();
                }
                return self.settle_pending(&live);
            }
            if let Some(evidence) = evidence::installed_evidence(&task, &live) {
                task.state = INSTALLED_PENDING_REBOOT.into();
                if let Some((uboot, _)) = evidence::loader_pair(&evidence) {
                    let original = evidence::actual_root(&task.before, &task.boot_slot)
                        .ok_or_else(|| {
                            Error::Unavailable("running rootfs no longer available".into())
                        })?;
                    task.target_slot = original.name.clone();
                    task.target_bootname = original.bootname.clone();
                    task.version = uboot.version.clone();
                    task.manifest_hash = uboot.manifest_hash.clone();
                    task.images = evidence
                        .iter()
                        .map(|s| (s.class.clone(), s.sha256.clone()))
                        .collect();
                }
                task.boot_slot = live.boot_slot.clone();
                task.primary = live.primary.clone();
                if let Some(slot) = evidence.iter().find(|slot| slot.name == task.target_slot) {
                    task.version = slot.version.clone();
                    task.manifest_hash = slot.manifest_hash.clone();
                }
                task.installed = evidence;
                if self.can_confirm(&task, &live) {
                    task.state = BOOT_CONFIRMED.into();
                }
                self.commit(task)?;
                if !alive {
                    self.release();
                }
                return self.settle_pending(&live);
            }
            if alive {
                self.last_progress = Instant::now();
            }
            let grace = self.last_progress.elapsed() < Duration::from_secs(5);
            if alive
                || grace
                    && (task.source == "hawkbit" && task.updater_exit.is_none()
                        || task.source == "local" && status.result.is_none())
            {
                return self.commit(task).map(drop);
            }
            if self
                .external
                .as_ref()
                .is_some_and(|external| external.id == task.id)
                && task.source == "hawkbit"
                && task.boot_id == self.boot_id
                && task.updater_exit == Some(0)
                && task.error.is_empty()
                && task.action_id.is_empty()
                && task.version.is_empty()
                && !task.observed_installing
                && task.result.is_none()
                && status.result.is_none()
                && task.installed.is_empty()
                && evidence::unchanged_check(&task, &live)
            {
                return self.discard_empty(task);
            }
            task.state = UNKNOWN.into();
            task.error = "no attributable completed RAUC installation; the request is never automatically resubmitted".into();
            if task.updater_exit.is_some_and(|code| code != 0) {
                task.state = FAILED.into();
                task.error = "updater process failed without verified installation evidence".into();
            }
            self.commit(task)?;
            self.release();
            return self.settle_pending(&live);
        }
        if status.operation == "idle"
            && self
                .history
                .iter()
                .any(|t| t.state == INSTALLED_PENDING_REBOOT && t.boot_id != self.boot_id)
        {
            let live = self.backend.snapshot(deadline)?;
            self.settle_pending(&live)?;
        }
        Ok(())
    }

    fn can_confirm(&self, task: &Task, live: &Snapshot) -> bool {
        if task.boot_id == self.boot_id
            || !evidence::same_installed_evidence(task, live)
            || !self.environment.boot_confirmed()
        {
            return false;
        }
        if evidence::bootloader_task(task) {
            return evidence::verified_loader_task(task) && evidence::unchanged_os(task, live);
        }
        evidence::actual_root(&live.slots, &live.boot_slot).is_some_and(|running| {
            running.name == task.target_slot && running.boot_status == "good"
        }) && live.primary == task.target_slot
            && !task.version.is_empty()
            && self.environment.running_version() == task.version
    }

    fn settle_pending(&mut self, live: &Snapshot) -> Result<()> {
        let running = evidence::actual_root(&live.slots, &live.boot_slot);
        let mut changes = Vec::new();
        for task in self
            .history
            .iter()
            .filter(|t| t.state == INSTALLED_PENDING_REBOOT)
        {
            let outcome = if self.can_confirm(task, live) {
                (BOOT_CONFIRMED, "")
            } else if evidence::superseded(task, live) {
                (FAILED, SUPERSEDED)
            } else if evidence::same_installed_evidence(task, live)
                && (evidence::bootloader_task(task) && evidence::unchanged_os(task, live)
                    || !evidence::bootloader_task(task)
                        && (task.boot_id == self.boot_id
                            || running.is_some_and(|root| root.name == task.target_slot)))
            {
                continue;
            } else if task.boot_id != self.boot_id
                && running.is_some_and(|root| evidence::rolled_back(task, live, root))
            {
                (FAILED, BOOT_ROLLED_BACK)
            } else {
                (UNKNOWN, BOOT_UNKNOWN)
            };
            let mut task = task.clone();
            task.state = outcome.0.into();
            task.error = outcome.1.into();
            task.boot_slot = live.boot_slot.clone();
            task.primary = live.primary.clone();
            changes.push(task);
        }
        for task in changes {
            self.commit(task)?;
        }
        Ok(())
    }
}

fn validate_request(path: &str, request_id: &str) -> Result<String> {
    if !Path::new(path).is_absolute()
        || path.len() > 4096
        || path.contains('\0')
        || !request_id.is_empty() && !valid_id(request_id)
    {
        return Err(Error::Invalid(
            "path must be absolute and request ID must be valid".into(),
        ));
    }
    let mut clean = PathBuf::from("/");
    for component in Path::new(path).components() {
        match component {
            Component::Normal(name) => clean.push(name),
            Component::ParentDir => {
                clean.pop();
            }
            _ => {}
        }
    }
    Ok(clean
        .into_os_string()
        .into_string()
        .expect("path came from a UTF-8 string"))
}

fn task_id() -> Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(Error::unavailable)?;
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}
