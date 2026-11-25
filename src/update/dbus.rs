use super::{
    Backend, Bundle, Deadline, Error, InstallError, Progress, Result, SlotEvidence, Snapshot,
    Status,
};
use futures_util::{FutureExt, StreamExt};
use std::{
    collections::{HashMap, HashSet},
    sync::mpsc,
    thread,
};
use tokio::runtime::Runtime;
use zbus::{
    Connection, Proxy,
    connection::Builder,
    message::Sequence,
    names::OwnedUniqueName,
    proxy::SignalStream,
    zvariant::{OwnedValue, Str, Value},
};

const BUS_SOCKET: &str = "unix:path=/run/dbus/system_bus_socket";
const SERVICE: &str = "de.pengutronix.rauc";
const INSTALLER: &str = "de.pengutronix.rauc.Installer";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const BUSY: &str = "Already processing a different method";

type Dict = HashMap<String, OwnedValue>;
type Work = Box<dyn FnOnce(&mut Native, &Runtime) + Send>;

pub(super) struct BackendClient {
    jobs: Option<mpsc::Sender<Work>>,
    thread: Option<thread::JoinHandle<()>>,
}

struct Endpoint {
    installer: Proxy<'static>,
    properties: Proxy<'static>,
    completed: SignalStream<'static>,
    observed: Sequence,
    boot_slot: String,
}

struct Native {
    address: String,
    endpoint: Option<Endpoint>,
    path: String,
    result: Option<i32>,
    completion_after: Option<Sequence>,
}

impl BackendClient {
    pub(super) fn spawn() -> Result<Self> {
        Self::spawn_on(BUS_SOCKET.to_owned())
    }

    fn spawn_on(address: String) -> Result<Self> {
        let (jobs, work) = mpsc::channel::<Work>();
        let (ready, started) = mpsc::sync_channel(1);
        let thread = thread::Builder::new()
            .name("naos-rauc-dbus".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let runtime = match runtime {
                    Ok(runtime) => {
                        if ready.send(Ok(())).is_err() {
                            return;
                        }
                        runtime
                    }
                    Err(error) => {
                        let _ = ready.send(Err(Error::unavailable(error)));
                        return;
                    }
                };
                // zbus drops its proxies and signal matches via the current
                // executor. Keep the runtime entered until Native is gone,
                // including thread shutdown after the last block_on call.
                let _runtime_context = runtime.enter();
                let mut native = Native {
                    address,
                    endpoint: None,
                    path: String::new(),
                    result: None,
                    completion_after: None,
                };
                for job in work {
                    job(&mut native, &runtime);
                }
            })
            .map_err(Error::unavailable)?;
        started.recv().map_err(Error::unavailable)??;
        Ok(Self {
            jobs: Some(jobs),
            thread: Some(thread),
        })
    }

    fn call<T: Send + 'static>(
        &self,
        deadline: Deadline,
        callback: impl FnOnce(&mut Native, &Runtime) -> T + Send + 'static,
    ) -> Result<T> {
        deadline.remaining()?;
        let (done, result) = mpsc::sync_channel(1);
        self.enqueue(Box::new(move |native, runtime| {
            let _ = done.send(callback(native, runtime));
        }))?;
        result
            .recv_timeout(deadline.remaining()?)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => Deadline::expired(),
                mpsc::RecvTimeoutError::Disconnected => worker_stopped(),
            })
    }

    fn enqueue(&self, work: Work) -> Result<()> {
        self.jobs
            .as_ref()
            .ok_or_else(worker_stopped)?
            .send(work)
            .map_err(|_| worker_stopped())
    }

    fn stop_worker(&mut self) -> thread::Result<()> {
        self.jobs.take();
        self.thread.take().map_or(Ok(()), thread::JoinHandle::join)
    }
}

impl Drop for BackendClient {
    fn drop(&mut self) {
        if self.stop_worker().is_err() {
            eprintln!("naos-core: RAUC D-Bus worker panicked");
        }
    }
}

impl Backend for BackendClient {
    fn status(&mut self, deadline: Deadline) -> Result<Status> {
        self.call(deadline, move |state, runtime| {
            runtime.block_on(state.status(deadline))
        })?
    }

    fn snapshot(&mut self, deadline: Deadline) -> Result<Snapshot> {
        self.call(deadline, move |state, runtime| {
            runtime.block_on(state.snapshot(deadline))
        })?
    }

    fn inspect(&mut self, path: &str, deadline: Deadline) -> Result<Bundle> {
        let path = path.to_owned();
        self.call(deadline, move |state, runtime| {
            runtime.block_on(state.inspect(&path, deadline))
        })?
    }

    fn install(
        &mut self,
        path: &str,
        id: &str,
        manifest: &str,
        deadline: Deadline,
    ) -> std::result::Result<(), InstallError> {
        deadline.remaining().map_err(InstallError::Rejected)?;
        let args = (path.to_owned(), id.to_owned(), manifest.to_owned());
        self.call(deadline, move |state, runtime| {
            runtime.block_on(state.install(&args.0, &args.1, &args.2, deadline))
        })
        .map_err(InstallError::Uncertain)?
    }

    fn watch_external(&mut self, path: &str, deadline: Deadline) -> Result<()> {
        let path = path.to_owned();
        self.call(deadline, move |state, _| {
            deadline.remaining()?;
            state.reserve(&path);
            Ok(())
        })?
    }

    fn release(&mut self) {
        let _ = self.enqueue(Box::new(|state, _| state.completion_after = None));
    }
}

impl Endpoint {
    async fn connect(address: &str) -> zbus::Result<Self> {
        let connection: Connection = Builder::address(address)?.build().await?;
        let bus = Proxy::new(
            &connection,
            "org.freedesktop.DBus",
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
        )
        .await?;
        let owner: OwnedUniqueName = bus.call("GetNameOwner", &(SERVICE,)).await?;
        // A unique peer destination cannot cause D-Bus to start another service.
        let installer = Proxy::new_owned(connection.clone(), owner.clone(), "/", INSTALLER).await?;
        let properties = Proxy::new_owned(connection.clone(), owner, "/", PROPERTIES).await?;
        let completed = installer.receive_signal("Completed").await?;
        Ok(Self {
            installer,
            properties,
            completed,
            observed: Sequence::default(),
            boot_slot: String::new(),
        })
    }
}

impl Native {
    fn reserve(&mut self, path: &str) {
        self.completion_after = self.endpoint.as_ref().map(|peer| peer.observed);
        self.path = path.to_owned();
        self.result = None;
    }

    fn disconnect(&mut self) {
        self.endpoint = None;
        self.completion_after = None;
        self.result = None;
    }

    async fn get_status(&mut self) -> Result<Status> {
        if self.endpoint.is_none() {
            self.endpoint = Some(
                Endpoint::connect(&self.address)
                    .await
                    .map_err(|error| Error::Unavailable(format!("connect RAUC: {error}")))?,
            );
        }
        let peer = self.endpoint.as_mut().expect("connected");
        let reply = peer
            .properties
            .call_method("GetAll", &(INSTALLER,))
            .await
            .map_err(Error::unavailable)?;
        peer.observed = reply.recv_position();
        let values: Dict = reply.body().deserialize().map_err(Error::unavailable)?;
        let mut status = decode_status(&values)?;
        peer.boot_slot = optional_text(&values, "BootSlot").to_owned();

        loop {
            match peer.completed.next().now_or_never() {
                Some(Some(signal)) => {
                    let result: i32 = signal.body().deserialize().map_err(Error::unavailable)?;
                    if self
                        .completion_after
                        .is_some_and(|after| signal.recv_position() > after)
                    {
                        self.completion_after = None;
                        self.result = Some(result);
                    }
                }
                Some(None) => {
                    return Err(Error::Unavailable("RAUC completion stream closed".into()));
                }
                None => break,
            }
        }
        status.pending = self.completion_after.is_some();
        status.path = self.path.clone();
        status.result = self.result;
        Ok(status)
    }

    async fn status(&mut self, deadline: Deadline) -> Result<Status> {
        let result = before_deadline(deadline, self.get_status()).await;
        match result {
            Ok(Ok(status)) => Ok(status),
            Ok(Err(error)) => {
                self.disconnect();
                Err(error)
            }
            Err(error) => {
                self.disconnect();
                Err(error)
            }
        }
    }

    async fn snapshot(&mut self, deadline: Deadline) -> Result<Snapshot> {
        let peer = self.endpoint.as_ref().ok_or_else(not_connected)?;
        if peer.boot_slot.is_empty() {
            return Err(Error::Unavailable("RAUC BootSlot is unavailable".into()));
        }
        before_deadline(deadline, async {
            let slots: Vec<(String, Dict)> = peer
                .installer
                .call("GetSlotStatus", &())
                .await
                .map_err(|error| Error::Unavailable(format!("RAUC slot status: {error}")))?;
            if !(2..=32).contains(&slots.len()) {
                return Err(Error::Unavailable("invalid RAUC slot count".into()));
            }
            let primary: String = peer
                .installer
                .call("GetPrimary", &())
                .await
                .map_err(|error| Error::Unavailable(format!("RAUC primary: {error}")))?;
            let mut names = HashSet::new();
            let mut evidence = Vec::with_capacity(slots.len());
            for (name, values) in slots {
                if name.is_empty() || !names.insert(name.clone()) {
                    return Err(Error::Unavailable("invalid RAUC slot identity".into()));
                }
                evidence.push(decode_slot(name, &values));
            }
            Ok(Snapshot {
                boot_slot: peer.boot_slot.clone(),
                primary,
                slots: evidence,
            })
        })
        .await?
    }

    async fn inspect(&mut self, path: &str, deadline: Deadline) -> Result<Bundle> {
        let peer = self.endpoint.as_ref().ok_or_else(not_connected)?;
        let values: Dict = before_deadline(
            deadline,
            peer.installer.call("InspectBundle", &(path, Dict::new())),
        )
        .await?
        .map_err(|error| Error::Unavailable(format!("RAUC verify bundle: {error}")))?;
        decode_bundle(values)
    }

    async fn install(
        &mut self,
        path: &str,
        id: &str,
        manifest: &str,
        deadline: Deadline,
    ) -> std::result::Result<(), InstallError> {
        deadline.remaining().map_err(InstallError::Rejected)?;
        self.reserve(path);
        let peer = self
            .endpoint
            .as_ref()
            .ok_or_else(|| InstallError::Uncertain(not_connected()))?;
        let options = Dict::from([
            ("transaction-id".into(), OwnedValue::from(Str::from(id))),
            (
                "require-manifest-hash".into(),
                OwnedValue::from(Str::from(manifest)),
            ),
        ]);
        let result = before_deadline(
            deadline,
            peer.installer
                .call::<_, _, ()>("InstallBundle", &(path, options)),
        )
        .await;
        match result {
            Ok(Ok(())) => Ok(()),
            Err(error) => Err(InstallError::Uncertain(error)),
            Ok(Err(ref error @ zbus::Error::MethodError(ref name, ref message, _)))
                if !matches!(
                    name.as_str(),
                    "org.freedesktop.DBus.Error.ServiceUnknown"
                        | "org.freedesktop.DBus.Error.NameHasNoOwner"
                        | "org.freedesktop.DBus.Error.Disconnected"
                        | "org.freedesktop.DBus.Error.NoReply"
                        | "org.freedesktop.DBus.Error.Timeout"
                ) =>
            {
                Err(InstallError::Rejected(
                    if message.as_deref() == Some(BUSY) {
                        Error::Busy
                    } else {
                        Error::unavailable(error)
                    },
                ))
            }
            Ok(Err(error)) => Err(InstallError::Uncertain(Error::unavailable(error))),
        }
    }
}

fn not_connected() -> Error {
    Error::Unavailable("RAUC D-Bus endpoint is unavailable".into())
}

fn worker_stopped() -> Error {
    Error::Unavailable("RAUC D-Bus worker stopped".into())
}

async fn before_deadline<F: Future>(deadline: Deadline, operation: F) -> Result<F::Output> {
    deadline.remaining()?;
    tokio::select! {
        biased;
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline.0)) => Err(Deadline::expired()),
        result = operation => Ok(result),
    }
}

fn unwrap_variant<'a, 'value>(mut value: &'a Value<'value>) -> &'a Value<'value> {
    while let Value::Value(inner) = value {
        value = inner;
    }
    value
}

fn optional_text<'a>(values: &'a Dict, name: &str) -> &'a str {
    text(values, name).unwrap_or_default()
}

fn text<'a>(values: &'a Dict, name: &str) -> Option<&'a str> {
    let Value::Str(value) = unwrap_variant(values.get(name)?) else {
        return None;
    };
    Some(value.as_str())
}

fn decode_status(values: &Dict) -> Result<Status> {
    let invalid = || Error::Unavailable("invalid RAUC status properties".into());
    let operation = text(values, "Operation")
        .filter(|value| !value.is_empty())
        .ok_or_else(invalid)?;
    let last_error = text(values, "LastError").ok_or_else(invalid)?;
    let Value::Structure(progress) = unwrap_variant(values.get("Progress").ok_or_else(invalid)?)
    else {
        return Err(invalid());
    };
    let [
        Value::I32(percentage),
        Value::Str(message),
        Value::I32(depth),
    ] = progress.fields()
    else {
        return Err(invalid());
    };
    Ok(Status {
        operation: operation.to_owned(),
        last_error: last_error.to_owned(),
        progress: Progress {
            percentage: *percentage,
            message: message.as_str().to_owned(),
            depth: *depth,
        },
        ..Status::default()
    })
}

fn decode_slot(name: String, values: &Dict) -> SlotEvidence {
    let field = |key| optional_text(values, key).to_owned();
    let count = values
        .get("installed.count")
        .map(|value| unwrap_variant(value))
        .and_then(|v| {
            if let Value::U32(count) = v {
                Some(*count)
            } else {
                None
            }
        })
        .unwrap_or_default();
    SlotEvidence {
        name,
        class: field("class"),
        parent: field("parent"),
        device: field("device"),
        bootname: field("bootname"),
        state: field("state"),
        boot_status: field("boot-status"),
        version: field("bundle.version"),
        manifest_hash: field("bundle.hash"),
        sha256: field("sha256"),
        status: field("status"),
        transaction: field("installed.transaction"),
        installed_at: field("installed.timestamp"),
        installed_count: count,
    }
}

fn decode_bundle(mut values: Dict) -> Result<Bundle> {
    let invalid = || Error::Unavailable("invalid RAUC manifest images".into());
    let mut bundle = Bundle {
        hash: optional_text(&values, "manifest-hash").to_owned(),
        ..Bundle::default()
    };
    if let Some(update) = values.remove("update") {
        let value = unwrap_variant(&update)
            .try_clone()
            .map_err(Error::unavailable)?;
        if let Ok(info) = Dict::try_from(value) {
            bundle.version = optional_text(&info, "version").to_owned();
        }
    }
    let images = values.remove("images").ok_or_else(invalid)?;
    let value = unwrap_variant(&images)
        .try_clone()
        .map_err(Error::unavailable)?;
    let images = Vec::<Dict>::try_from(value).map_err(|_| invalid())?;
    for image in images {
        let class = optional_text(&image, "slot-class");
        let digest = optional_text(&image, "checksum");
        if class.is_empty()
            || !super::evidence::checksum(digest)
            || bundle
                .images
                .insert(class.to_owned(), digest.to_owned())
                .is_some()
        {
            return Err(Error::Unavailable("ambiguous RAUC manifest images".into()));
        }
    }
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::CALL_TIMEOUT;
    use std::{
        process::{Child, Command, Stdio},
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
    use zbus::object_server::SignalEmitter;

    fn string(value: &str) -> OwnedValue {
        OwnedValue::from(Str::from(value))
    }

    fn progress() -> OwnedValue {
        OwnedValue::try_from(Value::from((35_i32, "Writing", 1_i32))).unwrap()
    }

    fn inspect_reply() -> Dict {
        let images = [
            Dict::from([
                ("slot-class".into(), string("rootfs")),
                ("checksum".into(), string(&"b".repeat(64))),
            ]),
            Dict::from([
                ("slot-class".into(), string("boot")),
                ("checksum".into(), string(&"c".repeat(64))),
            ]),
        ];
        Dict::from([
            ("manifest-hash".into(), string(&"a".repeat(64))),
            (
                "update".into(),
                OwnedValue::from(Dict::from([("version".into(), string("2.0.0"))])),
            ),
            (
                "images".into(),
                OwnedValue::try_from(Value::from(Vec::from(images))).unwrap(),
            ),
        ])
    }

    #[test]
    fn decodes_native_variant_dictionaries_and_checks_exact_status_shape() {
        let mut props = Dict::from([
            ("Operation".into(), string("idle")),
            ("LastError".into(), string("")),
            ("Progress".into(), progress()),
        ]);
        let decoded = decode_status(&props).unwrap();
        assert_eq!(decoded.operation, "idle");
        assert_eq!(
            decoded.progress,
            Progress {
                percentage: 35,
                message: "Writing".into(),
                depth: 1,
            }
        );
        props.insert("Progress".into(), string("35 Writing 1"));
        assert!(matches!(decode_status(&props), Err(Error::Unavailable(_))));
        let bundle = decode_bundle(inspect_reply()).unwrap();
        assert_eq!(bundle.version, "2.0.0");
        assert_eq!(bundle.hash, "a".repeat(64));
        assert_eq!(bundle.images["rootfs"], "b".repeat(64));
    }

    type InstallArguments = (String, String, String);

    struct FakeInstaller(Arc<Mutex<Vec<InstallArguments>>>);

    #[zbus::interface(name = "de.pengutronix.rauc.Installer")]
    impl FakeInstaller {
        #[zbus(property)]
        fn operation(&self) -> String {
            "idle".into()
        }

        #[zbus(property)]
        fn last_error(&self) -> String {
            String::new()
        }

        #[zbus(property)]
        fn boot_slot(&self) -> String {
            "A".into()
        }

        #[zbus(property)]
        fn progress(&self) -> (i32, String, i32) {
            (35, "Writing".into(), 1)
        }

        fn get_slot_status(&self) -> Vec<(String, Dict)> {
            [
                ("rootfs.0", "rootfs", "", "A", "booted"),
                ("boot.0", "boot", "rootfs.0", "", "active"),
                ("rootfs.1", "rootfs", "", "B", "inactive"),
                ("boot.1", "boot", "rootfs.1", "", "inactive"),
            ]
            .into_iter()
            .map(|(name, class, parent, bootname, state)| {
                (
                    name.into(),
                    Dict::from([
                        ("class".into(), string(class)),
                        ("parent".into(), string(parent)),
                        ("bootname".into(), string(bootname)),
                        ("state".into(), string(state)),
                        ("installed.count".into(), OwnedValue::from(5_u32)),
                    ]),
                )
            })
            .collect()
        }

        fn get_primary(&self) -> String {
            "rootfs.0".into()
        }

        fn inspect_bundle(&self, path: &str, options: Dict) -> zbus::fdo::Result<Dict> {
            if path != "/test.raucb" || !options.is_empty() {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "unexpected InspectBundle arguments".into(),
                ));
            }
            Ok(inspect_reply())
        }

        async fn install_bundle(
            &self,
            path: &str,
            options: Dict,
            #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        ) -> zbus::fdo::Result<()> {
            if options.len() != 2 {
                return Err(zbus::fdo::Error::InvalidArgs(
                    "unexpected InstallBundle options".into(),
                ));
            }
            self.0.lock().unwrap().push((
                path.into(),
                optional_text(&options, "transaction-id").into(),
                optional_text(&options, "require-manifest-hash").into(),
            ));
            Self::completed(&emitter, 0)
                .await
                .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
        }

        #[zbus(signal)]
        async fn completed(emitter: &SignalEmitter<'_>, result: i32) -> zbus::Result<()>;
    }

    struct SessionBus(Child);

    impl Drop for SessionBus {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    #[ignore = "requires dbus-daemon and permission to bind a private Unix socket"]
    fn native_rauc_api_round_trip_on_a_private_bus() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("dbus.sock");
        let address = format!("unix:path={}", socket.display());
        let mut daemon = SessionBus(
            Command::new("dbus-daemon")
                .args([
                    "--session",
                    "--nofork",
                    "--nopidfile",
                    "--address",
                    &address,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .spawn()
                .expect("launch private dbus-daemon"),
        );
        let deadline = Instant::now() + CALL_TIMEOUT;
        while !socket.exists() {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "private dbus-daemon stopped"
            );
            assert!(Instant::now() < deadline, "private bus did not bind");
            thread::sleep(Duration::from_millis(10));
        }

        let installed = Arc::new(Mutex::new(Vec::new()));
        let fake = FakeInstaller(Arc::clone(&installed));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (ready, started) = mpsc::sync_channel(1);
        let server_address = address.clone();
        let server = thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let connection = Builder::address(server_address.as_str())
                        .unwrap()
                        .name(SERVICE)
                        .unwrap()
                        .serve_at("/", fake)
                        .unwrap()
                        .build()
                        .await
                        .unwrap();
                    ready.send(()).unwrap();
                    let _ = stopped.await;
                    drop(connection);
                });
        });
        started.recv_timeout(CALL_TIMEOUT).unwrap();

        let mut client = BackendClient::spawn_on(address).unwrap();
        let before = client.status(Deadline::new()).unwrap();
        assert_eq!(before.operation, "idle");
        assert!(!before.pending);
        assert!(before.result.is_none());
        let snapshot = client.snapshot(Deadline::new()).unwrap();
        assert_eq!(snapshot.primary, "rootfs.0");
        assert_eq!(snapshot.boot_slot, "A");
        assert_eq!(snapshot.slots.len(), 4);
        assert_eq!(snapshot.slots[0].installed_count, 5);

        let bundle = client.inspect("/test.raucb", Deadline::new()).unwrap();
        assert_eq!(bundle.version, "2.0.0");
        let expired = Deadline(Instant::now());
        assert!(matches!(
            client.install("/test.raucb", "expired-transaction", &bundle.hash, expired),
            Err(InstallError::Rejected(Error::Unavailable(_)))
        ));
        assert!(installed.lock().unwrap().is_empty());
        match client.install(
            "/test.raucb",
            "test-transaction",
            &bundle.hash,
            Deadline::new(),
        ) {
            Ok(()) => {}
            Err(InstallError::Rejected(error) | InstallError::Uncertain(error)) => {
                panic!("private RAUC installation failed: {error}")
            }
        }
        let after = client.status(Deadline::new()).unwrap();
        assert!(!after.pending);
        assert_eq!(after.result, Some(0));
        assert_eq!(
            *installed.lock().unwrap(),
            vec![("/test.raucb".into(), "test-transaction".into(), bundle.hash)]
        );
        // Tear down the actor with its live RAUC subscription still present.
        // Check the join result because Drop is unable to propagate a panic.
        client
            .stop_worker()
            .expect("RAUC D-Bus worker should stop cleanly");
        drop(client);
        stop.send(()).unwrap();
        server.join().unwrap();
    }
}
