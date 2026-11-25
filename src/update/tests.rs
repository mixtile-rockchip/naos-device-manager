use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};

#[derive(Clone)]
struct FakeRauc(Arc<Mutex<RaucState>>);

#[derive(Clone, Copy)]
enum Submission {
    Accept,
    LoseReply,
    Reject,
}

struct RaucState {
    status: Status,
    live: Snapshot,
    bundle: Bundle,
    submissions: Vec<(String, String, String)>,
    submission: Submission,
    journal_path: PathBuf,
    stage_delay: Duration,
}

impl FakeRauc {
    fn observe_stage(&self, deadline: Deadline) -> Result<MutexGuard<'_, RaucState>> {
        deadline.remaining()?;
        let state = self.0.lock().unwrap();
        if !state.stage_delay.is_zero() {
            // Model a reply received late despite the backend's initial check.
            thread::sleep(state.stage_delay);
        }
        Ok(state)
    }
}

impl Backend for FakeRauc {
    fn status(&mut self, deadline: Deadline) -> Result<Status> {
        Ok(self.observe_stage(deadline)?.status.clone())
    }

    fn snapshot(&mut self, deadline: Deadline) -> Result<Snapshot> {
        Ok(self.observe_stage(deadline)?.live.clone())
    }

    fn inspect(&mut self, _: &str, deadline: Deadline) -> Result<Bundle> {
        Ok(self.observe_stage(deadline)?.bundle.clone())
    }

    fn install(
        &mut self,
        path: &str,
        id: &str,
        manifest: &str,
        _: Deadline,
    ) -> std::result::Result<(), InstallError> {
        let mut state = self.0.lock().unwrap();
        let journal: serde_json::Value =
            serde_json::from_slice(&fs::read(&state.journal_path).unwrap()).unwrap();
        assert!(journal["tasks"].as_array().unwrap().iter().any(|task| {
            task["id"] == id && task["state"] == SUBMITTED && task["manifest_hash"] == manifest
        }));
        state
            .submissions
            .push((path.into(), id.into(), manifest.into()));
        match state.submission {
            Submission::Accept => Ok(()),
            Submission::LoseReply => Err(InstallError::Uncertain(Error::Unavailable(
                "connection lost".into(),
            ))),
            Submission::Reject => Err(InstallError::Rejected(Error::Busy)),
        }
    }

    fn watch_external(&mut self, _: &str, deadline: Deadline) -> Result<()> {
        deadline.remaining().map(drop)
    }
    fn release(&mut self) {
        self.0.lock().unwrap().status.pending = false;
    }
}

#[derive(Default)]
struct EnvironmentState {
    updater: Option<bool>,
    confirmed: bool,
    running_version: String,
}

struct FakeEnvironment(Arc<Mutex<EnvironmentState>>);

impl Environment for FakeEnvironment {
    fn process_state(&self, _: &ExternalIdentity, _: &str) -> (bool, bool) {
        self.updater_present()
    }
    fn updater_present(&self) -> (bool, bool) {
        self.0
            .lock()
            .unwrap()
            .updater
            .map_or((false, false), |alive| (alive, true))
    }
    fn boot_confirmed(&self) -> bool {
        self.0.lock().unwrap().confirmed
    }
    fn running_version(&self) -> String {
        self.0.lock().unwrap().running_version.clone()
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    bundle: PathBuf,
    rauc: FakeRauc,
    environment: Arc<Mutex<EnvironmentState>>,
    gate: Gate,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let bundle = directory.path().join("release.raucb");
        fs::write(&bundle, b"fake bundle").unwrap();
        let rauc = FakeRauc(Arc::new(Mutex::new(RaucState {
            status: Status {
                operation: "idle".into(),
                ..Status::default()
            },
            live: baseline(),
            bundle: Bundle {
                version: "2.0.0".into(),
                hash: "a".repeat(64),
                images: system_images(),
            },
            submissions: Vec::new(),
            submission: Submission::Accept,
            journal_path: directory.path().join(HISTORY),
            stage_delay: Duration::ZERO,
        })));
        let environment = Arc::new(Mutex::new(EnvironmentState {
            updater: Some(false),
            ..EnvironmentState::default()
        }));
        Self {
            directory,
            bundle,
            rauc,
            environment,
            gate: Gate::new(),
        }
    }

    fn open(&self, boot: &str) -> Client {
        Client::with_backend(
            self.directory.path(),
            self.gate.clone(),
            boot.into(),
            Box::new(self.rauc.clone()),
            Box::new(FakeEnvironment(Arc::clone(&self.environment))),
            false,
        )
        .unwrap()
    }

    fn path(&self) -> &str {
        self.bundle.to_str().unwrap()
    }

    fn set_installed(&self, task: &Task, bootloader: bool) {
        let mut rauc = self.rauc.0.lock().unwrap();
        rauc.status.result = Some(0);
        if !bootloader {
            rauc.live.primary = task.target_slot.clone();
        }
        let mut images = task.images.clone();
        if images.is_empty() {
            images = system_images();
        }
        let hash = if task.manifest_hash.is_empty() {
            "a".repeat(64)
        } else {
            task.manifest_hash.clone()
        };
        let version = if task.version.is_empty() {
            "2.0.0"
        } else {
            &task.version
        };
        for slot in &mut rauc.live.slots {
            if slot.name == task.target_slot && !bootloader
                || slot.parent == task.target_slot && !bootloader
                || bootloader && matches!(slot.class.as_str(), "uboot" | "idb")
            {
                slot.version = version.into();
                slot.manifest_hash = hash.clone();
                slot.sha256 = images[&slot.class].clone();
                slot.status = "ok".into();
                slot.transaction = task.id.clone();
                slot.installed_at = task.created_at.to_rfc3339();
                slot.installed_count += 1;
            }
        }
    }

    fn request(&self, id: &str) -> ExternalRequest {
        ExternalRequest {
            path: self.path().into(),
            request_id: id.into(),
            ..ExternalRequest::default()
        }
    }
}

fn root(slot: u8) -> SlotEvidence {
    SlotEvidence {
        name: format!("rootfs.{slot}"),
        class: "rootfs".into(),
        device: format!("/dev/naos/rootfs_{slot}"),
        bootname: if slot == 0 { "A".into() } else { "B".into() },
        state: if slot == 0 {
            "booted".into()
        } else {
            "inactive".into()
        },
        boot_status: "good".into(),
        ..SlotEvidence::default()
    }
}

fn boot(slot: u8) -> SlotEvidence {
    SlotEvidence {
        name: format!("boot.{slot}"),
        class: "boot".into(),
        parent: format!("rootfs.{slot}"),
        device: format!("/dev/naos/boot_{slot}"),
        state: "inactive".into(),
        ..SlotEvidence::default()
    }
}

fn loader(class: &str, device: &str) -> SlotEvidence {
    SlotEvidence {
        name: format!("{class}.0"),
        class: class.into(),
        device: device.into(),
        state: "inactive".into(),
        ..SlotEvidence::default()
    }
}

fn baseline() -> Snapshot {
    Snapshot {
        boot_slot: "A".into(),
        primary: "rootfs.0".into(),
        slots: vec![
            root(0),
            boot(0),
            root(1),
            boot(1),
            loader("uboot", "/dev/naos/uboot"),
            loader("idb", "/dev/naos/disk"),
        ],
    }
}

fn system_images() -> Images {
    Images::from([
        ("rootfs".into(), "b".repeat(64)),
        ("boot".into(), "c".repeat(64)),
    ])
}

fn reconcile(client: &Client) {
    let mut state = client.lock().unwrap();
    let deadline = Deadline::new();
    let status = state.backend.status(deadline).unwrap();
    state.reconcile(status, deadline).unwrap();
}

fn timed_out<T>(outcome: Result<T>) -> bool {
    matches!(outcome, Err(Error::Unavailable(message)) if message == "RAUC request timed out")
}

#[test]
fn expiry_after_cumulative_slow_stages_prevents_an_install() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    fixture.rauc.0.lock().unwrap().stage_delay = Duration::from_millis(30);
    let deadline = Deadline(Instant::now() + Duration::from_millis(75));
    assert!(timed_out(client.install_until(
        fixture.path(),
        "expired",
        deadline
    )));
    assert!(fixture.rauc.0.lock().unwrap().submissions.is_empty());
    assert!(client.tasks().unwrap().is_empty());
    assert!(fixture.gate.active().is_none());
}

#[test]
fn expiry_while_waiting_for_admission_prevents_an_install() {
    let fixture = Fixture::new();
    let client = Arc::new(fixture.open("boot-1"));
    let state = client.lock().unwrap();
    let path = fixture.path().to_owned();
    let (sender, receiver) = mpsc::channel();
    let worker = {
        let client = client.clone();
        thread::spawn(move || {
            let deadline = Deadline(Instant::now() + Duration::from_millis(35));
            let _ = sender.send(client.install_until(&path, "blocked", deadline));
        })
    };
    let outcome = receiver.recv_timeout(Duration::from_secs(1));
    drop(state);
    worker.join().unwrap();
    assert!(timed_out(
        outcome.expect("admission waited beyond its budget")
    ));
    assert!(fixture.rauc.0.lock().unwrap().submissions.is_empty());
    assert!(client.tasks().unwrap().is_empty());
}

#[test]
fn expiry_before_external_reservation_leaves_no_pending_task() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    fixture.rauc.0.lock().unwrap().stage_delay = Duration::from_millis(30);
    let deadline = Deadline(Instant::now() + Duration::from_millis(45));
    assert!(timed_out(
        client.begin_external_until(fixture.request("expired"), deadline)
    ));
    assert!(!fixture.directory.path().join(PENDING).exists());
    assert!(client.tasks().unwrap().is_empty());
    assert!(fixture.gate.active().is_none());
}

#[test]
fn expiry_before_auto_reboot_releases_its_lease() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client
        .install_request(fixture.path(), "valid-install")
        .unwrap();
    fixture.set_installed(&task, false);
    reconcile(&client);
    fixture.rauc.0.lock().unwrap().stage_delay = Duration::from_millis(30);
    let deadline = Deadline(Instant::now() + Duration::from_millis(45));
    assert!(timed_out(client.prepare_auto_reboot_until("", deadline)));
    assert!(fixture.gate.active().is_none());
}

#[test]
fn durable_submission_pins_manifest_and_retries_are_idempotent_after_restart() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client.install_request(fixture.path(), "request-1").unwrap();
    assert_eq!(task.state, INSTALLING);
    assert_eq!(fixture.gate.active().as_deref(), Some("local-update"));
    assert_eq!(client.tasks().unwrap(), vec![task.clone()]);
    drop(client);
    fs::remove_file(&fixture.bundle).unwrap();

    let restarted = fixture.open("boot-1");
    assert_eq!(fixture.gate.active().as_deref(), Some("update-recovery"));
    assert_eq!(
        restarted
            .install_request(fixture.path(), "request-1")
            .unwrap(),
        task
    );
    assert!(matches!(
        restarted.install_request("/other.raucb", "request-1"),
        Err(Error::Conflict)
    ));
    assert!(matches!(
        restarted.install_request("/other.raucb", "request-2"),
        Err(Error::Busy)
    ));
    let rauc = fixture.rauc.0.lock().unwrap();
    assert_eq!(
        rauc.submissions,
        vec![(fixture.path().into(), task.id, task.manifest_hash)]
    );
    assert_eq!(
        fs::metadata(fixture.directory.path().join(HISTORY))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn validates_local_file_identifier_and_exact_signed_manifest_shape() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    assert!(matches!(
        client.install_request("relative.raucb", ""),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        client.install_request(fixture.path(), "invalid id"),
        Err(Error::Invalid(_))
    ));
    let linked = fixture.directory.path().join("link.raucb");
    symlink(&fixture.bundle, &linked).unwrap();
    assert!(matches!(
        client.install_request(linked.to_str().unwrap(), ""),
        Err(Error::Invalid(_))
    ));
    fixture
        .rauc
        .0
        .lock()
        .unwrap()
        .bundle
        .images
        .insert("other".into(), "d".repeat(64));
    assert!(matches!(
        client.install_request(fixture.path(), "invalid-images"),
        Err(Error::Unavailable(_))
    ));
    assert!(client.tasks().unwrap().is_empty());
    assert!(fixture.rauc.0.lock().unwrap().submissions.is_empty());
    assert!(fixture.gate.active().is_none());
}

#[test]
fn never_submits_when_the_task_cannot_be_saved() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    fs::create_dir(fixture.directory.path().join(HISTORY)).unwrap();
    assert!(matches!(
        client.install_request(fixture.path(), ""),
        Err(Error::Persistence(_))
    ));
    assert!(fixture.rauc.0.lock().unwrap().submissions.is_empty());
    assert!(fixture.gate.active().is_none());
    assert!(matches!(client.tasks(), Err(Error::Persistence(_))));
}

#[test]
fn lost_install_reply_retains_guard_and_recovers_using_signed_evidence() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    fixture.rauc.0.lock().unwrap().submission = Submission::LoseReply;
    assert!(matches!(
        client.install_request(fixture.path(), "lost-reply"),
        Err(Error::Unavailable(_))
    ));
    let task = client.tasks().unwrap().remove(0);
    assert_eq!(task.state, SUBMITTED);
    assert!(task.error.contains("acceptance is uncertain"));
    assert!(client.busy().unwrap());
    drop(client);
    let restarted = fixture.open("boot-1");
    fixture.set_installed(&task, false);
    reconcile(&restarted);
    let settled = restarted.task(&task.id).unwrap();
    assert_eq!(settled.state, INSTALLED_PENDING_REBOOT);
    assert_eq!(fixture.rauc.0.lock().unwrap().submissions.len(), 1);
}

#[test]
fn definite_rejection_persists_failure_and_releases_gate() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    fixture.rauc.0.lock().unwrap().submission = Submission::Reject;
    assert!(matches!(
        client.install_request(fixture.path(), "rejection"),
        Err(Error::Busy)
    ));
    let task = client.tasks().unwrap().remove(0);
    assert_eq!(task.state, FAILED);
    assert!(fixture.gate.active().is_none());
}

#[test]
fn auto_reboot_requires_fresh_matching_evidence_a_clear_gate_and_no_updater() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client.install_request(fixture.path(), "local").unwrap();
    fixture.set_installed(&task, false);
    reconcile(&client);
    assert_eq!(
        client.task(&task.id).unwrap().state,
        INSTALLED_PENDING_REBOOT
    );

    let competitor = fixture.gate.try_acquire("recovery").unwrap();
    assert!(client.prepare_auto_reboot("").unwrap().is_none());
    drop(competitor);

    fixture.environment.lock().unwrap().updater = None;
    assert!(client.prepare_auto_reboot("").unwrap().is_none());
    fixture.environment.lock().unwrap().updater = Some(false);
    assert!(client.prepare_auto_reboot(&task.id).unwrap().is_none());
    let (verified, lease) = client.prepare_auto_reboot("").unwrap().unwrap();
    assert_eq!(verified.id, task.id);
    assert_eq!(fixture.gate.active().as_deref(), Some("update-auto-reboot"));
    drop(lease);

    fixture.rauc.0.lock().unwrap().live.slots[2].sha256 = "f".repeat(64);
    assert!(client.prepare_auto_reboot("").unwrap().is_none());
}

#[test]
fn complete_install_is_confirmed_only_on_matching_new_boot_after_openrc() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client.install_request(fixture.path(), "confirm").unwrap();
    fixture.set_installed(&task, false);
    reconcile(&client);
    drop(client);

    {
        let mut rauc = fixture.rauc.0.lock().unwrap();
        rauc.live.boot_slot = "B".into();
        rauc.live.slots[0].state = "inactive".into();
        rauc.live.slots[2].state = "booted".into();
        rauc.live.slots[2].transaction.clear();
        rauc.live.slots[3].transaction.clear();
    }
    let rebooted = fixture.open("boot-2");
    reconcile(&rebooted);
    assert_eq!(
        rebooted.task(&task.id).unwrap().state,
        INSTALLED_PENDING_REBOOT
    );
    fixture.environment.lock().unwrap().confirmed = true;
    fixture.environment.lock().unwrap().running_version = "wrong".into();
    reconcile(&rebooted);
    assert_eq!(
        rebooted.task(&task.id).unwrap().state,
        INSTALLED_PENDING_REBOOT
    );
    fixture.environment.lock().unwrap().running_version = "2.0.0".into();
    reconcile(&rebooted);
    assert_eq!(rebooted.task(&task.id).unwrap().state, BOOT_CONFIRMED);
    assert!(rebooted.prepare_auto_reboot("").unwrap().is_none());
}

#[test]
fn failed_target_boot_is_attributed_as_rollback() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client
        .install_request(fixture.path(), "rolled-back")
        .unwrap();
    fixture.set_installed(&task, false);
    reconcile(&client);
    drop(client);
    {
        let mut rauc = fixture.rauc.0.lock().unwrap();
        rauc.live.primary = "rootfs.0".into();
        rauc.live.slots[2].boot_status = "bad".into();
    }
    let restarted = fixture.open("boot-2");
    reconcile(&restarted);
    let failed = restarted.task(&task.id).unwrap();
    assert_eq!(failed.state, FAILED);
    assert_eq!(failed.error, BOOT_ROLLED_BACK);
}

#[test]
fn shared_bootloader_bundle_leaves_os_pair_and_primary_unchanged() {
    let fixture = Fixture::new();
    fixture.rauc.0.lock().unwrap().bundle.images = Images::from([
        ("uboot".into(), "b".repeat(64)),
        ("idb".into(), "c".repeat(64)),
    ]);
    let client = fixture.open("boot-1");
    let task = client.install_request(fixture.path(), "loader").unwrap();
    assert_eq!(task.target_slot, "rootfs.0");
    fixture.set_installed(&task, true);
    reconcile(&client);

    let pending = client.task(&task.id).unwrap();
    assert_eq!(pending.state, INSTALLED_PENDING_REBOOT);
    assert!(evidence::loader_pair(&pending.installed).is_some());
    assert_eq!(
        client.prepare_auto_reboot("").unwrap().unwrap().0.id,
        task.id
    );
    fixture.rauc.0.lock().unwrap().live.slots[2].boot_status = "bad".into();
    assert!(client.prepare_auto_reboot("").unwrap().is_none());
}

#[test]
fn empty_hawkbit_check_does_not_pollute_history() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client.begin_external(fixture.request("empty")).unwrap();
    assert_eq!(task.state, RUNNING_EXTERNAL);
    assert!(fixture.directory.path().join(PENDING).exists());
    assert_eq!(client.tasks().unwrap().len(), 1);
    let completed = client.end_external(&task.id, 0, None).unwrap();
    assert_eq!(completed.error, EMPTY_CHECK);
    assert!(client.tasks().unwrap().is_empty());
    assert!(!fixture.directory.path().join(PENDING).exists());
    assert!(fixture.gate.active().is_none());
}

#[test]
fn external_update_requires_new_evidence_and_durable_end_before_reboot() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client.begin_external(fixture.request("download")).unwrap();
    fixture.environment.lock().unwrap().updater = Some(true);
    fixture.set_installed(&task, false);
    reconcile(&client);
    assert_eq!(
        client.task(&task.id).unwrap().state,
        INSTALLED_PENDING_REBOOT
    );
    assert!(client.prepare_auto_reboot("").unwrap().is_none());
    fixture.environment.lock().unwrap().updater = Some(false);
    assert_eq!(
        client.end_external(&task.id, 0, None).unwrap().updater_exit,
        Some(0)
    );
    assert!(client.prepare_auto_reboot("").unwrap().is_some());
    assert!(!fixture.directory.path().join(PENDING).exists());
}

#[test]
fn attaches_only_the_expected_live_process_and_private_config_digest() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client
        .begin_external(fixture.request("identified"))
        .unwrap();
    fixture.environment.lock().unwrap().updater = Some(true);
    let config = fixture.directory.path().join("updater.cfg");
    fs::write(&config, b"[client]\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let mut identity = ExternalIdentity {
        pid: 1,
        boot_id: "boot-1".into(),
        start_time: 8,
        exe: "/usr/bin/rauc-hawkbit-updater".into(),
        config_path: config.to_str().unwrap().into(),
        config_sha256: "f".repeat(64),
    };
    assert!(matches!(
        client.attach_external(&task.id, identity.clone()),
        Err(Error::Invalid(_))
    ));
    identity.config_sha256 = process::config_digest(&identity.config_path).unwrap();
    client.attach_external(&task.id, identity.clone()).unwrap();
    assert_eq!(
        client.task(&task.id).unwrap().external_identity,
        Some(identity)
    );
}

#[test]
fn reads_existing_go_format_1_journal_and_rejects_unsafe_modes_and_tampering() {
    let fixture = Fixture::new();
    let file = fixture.directory.path().join(HISTORY);
    let history = r#"{
  "format": 1,
  "tasks": [{
    "id": "old-go-task",
    "source": "local",
    "state": "unknown",
    "created_at": "2026-11-17T03:09:26.100000001Z",
    "updated_at": "2026-11-17T03:10:01Z",
    "boot_id": "boot-old",
    "target_slot": "rootfs.1",
    "target_bootname": "B",
    "progress": {"percentage": 0, "message": "", "depth": 0},
    "result": null,
    "before": null,
    "error": "BOOT_RESULT_UNKNOWN: historical"
  }]
}
"#;
    fs::write(&file, history).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let client = fixture.open("boot-new");
    let task = client.task("old-go-task").unwrap();
    assert_eq!(task.created_at.timestamp_subsec_nanos(), 100_000_001);
    assert!(task.before.is_empty());
    assert!(fixture.gate.active().is_none());
    drop(client);

    let store = Store::open(fixture.directory.path()).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        store.read(HISTORY, MAX_TASKS),
        Err(Error::Persistence(_))
    ));
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&file, format!("{history}{{}}")).unwrap();
    assert!(matches!(
        store.read(HISTORY, MAX_TASKS),
        Err(Error::Persistence(_))
    ));
    fs::remove_file(&file).unwrap();
    symlink(&fixture.bundle, &file).unwrap();
    assert!(matches!(
        store.read(HISTORY, MAX_TASKS),
        Err(Error::Persistence(_))
    ));
}

#[test]
fn pending_sidecar_promotion_is_recovered_without_duplicate_or_install() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let mut task = client.begin_external(fixture.request("pending")).unwrap();
    task.state = FAILED.into();
    drop(client);

    let store = Store::open(fixture.directory.path()).unwrap();
    store.save(PENDING, std::slice::from_ref(&task), 1).unwrap();
    store
        .save(HISTORY, std::slice::from_ref(&task), MAX_TASKS)
        .unwrap();
    let restarted = fixture.open("boot-1");
    assert_eq!(restarted.tasks().unwrap(), vec![task]);
    assert!(!fixture.directory.path().join(PENDING).exists());
    assert!(fixture.rauc.0.lock().unwrap().submissions.is_empty());
}

#[test]
fn restores_go_format_1_bootloader_evidence_and_confirms_after_new_boot() {
    let fixture = Fixture::new();
    let sha_uboot = "b".repeat(64);
    let sha_idb = "c".repeat(64);
    let manifest = "a".repeat(64);
    let record = |class: &str, path: &str, sha: &str| {
        serde_json::json!({
            "name": format!("{class}.0"),
            "class": class,
            "device": path,
            "state": "inactive",
            "version": "2.0.0",
            "manifest_hash": manifest,
            "sha256": sha,
            "status": "ok",
            "transaction": "go-loader-update",
            "installed_at": "2026-11-17T03:09:31Z",
            "installed_count": 1,
        })
    };
    let history = serde_json::json!({
        "format": 1,
        "tasks": [{
            "id": "go-loader-update",
            "source": "local",
            "state": "installed_pending_reboot",
            "created_at": "2026-11-17T03:09:26.100000001Z",
            "updated_at": "2026-11-17T03:10:01Z",
            "version": "2.0.0",
            "manifest_hash": manifest,
            "images": {"uboot": sha_uboot, "idb": sha_idb},
            "boot_id": "boot-old",
            "target_slot": "rootfs.0",
            "target_bootname": "A",
            "boot_slot": "A",
            "primary": "rootfs.0",
            "progress": {"percentage": 100, "message": "", "depth": 1},
            "result": 0,
            "before": baseline().slots,
            "installed": [
                record("uboot", "/dev/naos/uboot", &sha_uboot),
                record("idb", "/dev/naos/disk", &sha_idb)
            ]
        }]
    });
    let file = fixture.directory.path().join(HISTORY);
    fs::write(&file, serde_json::to_vec_pretty(&history).unwrap()).unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
    let client = fixture.open("boot-new");
    let task = client.task("go-loader-update").unwrap();
    assert!(evidence::verified_loader_task(&task));
    assert_eq!(task.state, INSTALLED_PENDING_REBOOT);
    assert_eq!(task.result, Some(0));

    {
        let mut rauc = fixture.rauc.0.lock().unwrap();
        for record in &task.installed {
            let slot = rauc
                .live
                .slots
                .iter_mut()
                .find(|slot| slot.name == record.name)
                .unwrap();
            *slot = record.clone();
            slot.transaction.clear();
        }
    }
    fixture.environment.lock().unwrap().confirmed = true;
    reconcile(&client);
    assert_eq!(client.task(&task.id).unwrap().state, BOOT_CONFIRMED);
}

#[test]
fn refuses_root_parent_paths_and_symlinked_state_directory() {
    assert!(matches!(
        Store::open(Path::new("/")),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        Store::open(Path::new("/tmp/..")),
        Err(Error::Invalid(_))
    ));
    let fixture = Fixture::new();
    let link = fixture.directory.path().join("link");
    symlink(fixture.directory.path(), &link).unwrap();
    assert!(matches!(
        Store::open(&link.join("")),
        Err(Error::Persistence(_))
    ));
}

#[test]
fn concurrent_requests_with_the_same_id_submit_exactly_once() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let path = fixture.path();
    let barrier = std::sync::Barrier::new(2);
    let (first, second) = thread::scope(|scope| {
        let install = || {
            barrier.wait();
            client.install_request(path, "concurrent-retry").unwrap()
        };
        let first = scope.spawn(install);
        let second = scope.spawn(install);
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(first, second);
    assert_eq!(fixture.rauc.0.lock().unwrap().submissions.len(), 1);
}

#[test]
fn newer_complete_slot_pair_supersedes_saved_pending_install() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client
        .install_request(fixture.path(), "superseded")
        .unwrap();
    fixture.set_installed(&task, false);
    reconcile(&client);
    drop(client);

    {
        let mut rauc = fixture.rauc.0.lock().unwrap();
        for slot in rauc
            .live
            .slots
            .iter_mut()
            .filter(|slot| slot.name == task.target_slot || slot.parent == task.target_slot)
        {
            slot.transaction.clear();
            slot.manifest_hash = "f".repeat(64);
            slot.version = "3.0.0".into();
            slot.installed_count += 1;
        }
    }
    let restarted = fixture.open("boot-2");
    reconcile(&restarted);
    let outcome = restarted.task(&task.id).unwrap();
    assert_eq!(outcome.state, FAILED);
    assert_eq!(outcome.error, SUPERSEDED);
}

#[test]
fn successful_hawkbit_exit_without_complete_changed_evidence_is_not_install_proof() {
    let fixture = Fixture::new();
    let client = fixture.open("boot-1");
    let task = client.begin_external(fixture.request("partial")).unwrap();
    fixture.set_installed(&task, false);
    {
        let mut rauc = fixture.rauc.0.lock().unwrap();
        rauc.status.result = None;
        rauc.live.slots[3].transaction.clear();
    }
    let result = client.end_external(&task.id, 0, None).unwrap();
    assert_eq!(result.state, UNKNOWN);
    assert_ne!(result.error, EMPTY_CHECK);
    assert!(client.prepare_auto_reboot("").unwrap().is_none());
    assert_eq!(client.tasks().unwrap().len(), 1);
}
