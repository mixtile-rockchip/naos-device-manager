use crate::api::{self, Backend, Reload};
use crate::config::{self, Config};
use crate::container;
use crate::device::{self, OemError, OemInfo, RecoveryError, SystemInfo};
use crate::hawkbit;
use crate::mdns;
use crate::operation::{Gate, Lease};
use crate::server::{Bound, UnixClient};
use crate::update;
use crate::watchdog;
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::watch;

struct DeviceBackend {
    soc: String,
    config: PathBuf,
    containers: container::Client,
    updates: Arc<update::Client>,
    cloud: Arc<hawkbit::Manager>,
    discovery: mdns::Manager,
}

impl Backend for DeviceBackend {
    fn system(&self) -> Result<SystemInfo> {
        device::system_info(Path::new(device::VERSION_FILE))
    }

    fn oem(&self) -> Result<OemInfo, OemError> {
        OemInfo::read(&self.soc)
    }

    fn containers(&self) -> Result<Vec<container::State>, container::Error> {
        self.containers.list()
    }

    fn container_change(&self, id: &str, action: &str) -> Result<(), container::Error> {
        self.containers.change(id, action)
    }

    fn tasks(&self) -> update::Result<Vec<update::Task>> {
        self.updates.tasks()
    }

    fn task(&self, id: &str) -> update::Result<update::Task> {
        self.updates.task(id)
    }

    fn install(&self, path: &str, id: &str) -> update::Result<update::Task> {
        self.updates.install_request(path, id)
    }

    fn update_status(&self) -> update::Result<update::Status> {
        self.updates.status()
    }

    fn update_busy(&self) -> update::Result<bool> {
        self.updates.busy()
    }

    fn hawkbit(&self) -> Value {
        serde_json::to_value(self.cloud.snapshot()).expect("hawkBit status is serializable")
    }

    fn reload(&self, path: &Path) -> Result<Reload> {
        ensure!(
            path == self.config,
            "registration uses a different Core configuration"
        );
        let next = Config::load(&self.config)?.hawkbit;
        ensure!(
            next.enabled && !next.controller_id.is_empty(),
            "invalid saved hawkBit registration"
        );
        let result = self.cloud.reload(next)?;
        Ok(Reload {
            applied: result.applied,
            pending: result.pending,
        })
    }

    fn mdns_put(
        &self,
        id: &str,
        service: mdns::Service,
    ) -> Result<mdns::Registration, mdns::Error> {
        self.discovery.put(id, service)
    }

    fn mdns_list(&self) -> Result<Vec<mdns::Registration>, mdns::Error> {
        self.discovery.list()
    }

    fn mdns_delete(&self, id: &str) -> Result<(), mdns::Error> {
        self.discovery.delete(id)
    }

    fn recovery(&self) -> Result<(), RecoveryError> {
        device::request_recovery()
    }

    fn reboot(&self) -> Result<()> {
        device::reboot()
    }
}

struct CloudUpdates(Arc<update::Client>);

impl hawkbit::UpdateHooks for CloudUpdates {
    fn begin_external(&self, request: hawkbit::ExternalRequest) -> Result<String> {
        let task = self.0.begin_external(update::ExternalRequest {
            source: "hawkbit".into(),
            request_id: request.request_id,
            path: request.path,
            action_id: request.action_id,
            version: request.version,
        })?;
        Ok(task.id)
    }

    fn attach_external(&self, id: &str, identity: hawkbit::ProcessIdentity) -> Result<()> {
        self.0.attach_external(
            id,
            update::ExternalIdentity {
                pid: identity.pid,
                start_time: identity.start_time,
                boot_id: identity.boot_id,
                exe: identity.exe,
                config_path: identity.config_path,
                config_sha256: identity.config_sha256,
            },
        )?;
        Ok(())
    }

    fn end_external(&self, id: &str, exit_code: i32, error: Option<&str>) -> Result<()> {
        self.0.end_external(id, exit_code, error)?;
        Ok(())
    }
}

pub fn run(mut config: Config, path: &Path) -> Result<()> {
    // Private files and sockets are created before serving or starting workers.
    unsafe { libc::umask(0o077) };
    for warning in &config.warnings {
        eprintln!("naos-core: {warning}");
    }
    let oem = match device::apply_hostname(&config.soc) {
        Ok(oem) => Some(oem),
        Err(error) => {
            eprintln!("naos-core: OEM hostname: {error}");
            None
        }
    };
    let path = api::clean_absolute(&std::path::absolute(path)?);
    if config.hawkbit.enabled && config.hawkbit.controller_id.is_empty() {
        config.hawkbit.controller_id =
            config::ensure_controller_id(&path, || device::controller_id(&config.soc, oem))
                .context("hawkBit identity")?;
    }

    let gate = Gate::new();
    let updates = Arc::new(
        update::Client::open(&config.state_dir, gate.clone()).context("update task storage")?,
    );
    let cloud = Arc::new(hawkbit::Manager::new(
        config.hawkbit.clone(),
        Arc::new(CloudUpdates(updates.clone())),
    )?);
    let backend = Arc::new(DeviceBackend {
        soc: config.soc.clone(),
        config: path,
        containers: container::Client {
            socket: config.container_socket.clone(),
        },
        updates,
        cloud,
        discovery: mdns::Manager::default(),
    });
    // reqwest's blocking client owns its own runtime and is built/dropped here.
    let health_client = if config.watchdog.enabled {
        Some(UnixClient::new(
            Path::new(&config.socket),
            Duration::from_secs(2),
        )?)
    } else {
        None
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_services(config, backend, gate, health_client))
}

async fn run_services(
    config: Config,
    backend: Arc<DeviceBackend>,
    gate: Gate,
    health_client: Option<UnixClient>,
) -> Result<()> {
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let http = config.http_enabled.then_some(config.http_listen.as_str());
    let bound = Bound::bind(Path::new(&config.socket), http).await?;
    let router = api::router(backend.clone(), gate);
    let (stop_http, http_shutdown) = watch::channel(false);
    let mut server = tokio::spawn(async move { bound.run(router, http_shutdown).await });
    backend.cloud.start()?;
    let dog = watchdog::Manager::start(&config.watchdog, move || {
        let client = health_client
            .as_ref()
            .context("missing watchdog HTTP client")?;
        let response = client.request(reqwest::Method::GET, "/healthz", None)?;
        let status = response.status();
        crate::server::read_response(response, 256)?;
        ensure!(
            status == reqwest::StatusCode::OK,
            "Core health status {status}"
        );
        Ok(())
    })?;

    let (stop_background, background_shutdown) = watch::channel(false);
    let startup = tokio::spawn(start_container(config.clone(), background_shutdown.clone()));
    let reboot = tokio::spawn(auto_reboot(backend.updates.clone(), background_shutdown));
    eprintln!(
        "naos-core: serving Unix socket {}; TCP HTTP listener: {}; startup confirmation remains naos-boot-confirm",
        config.socket,
        http.unwrap_or("disabled")
    );
    let exit = tokio::select! {
        _ = interrupt.recv() => Exit::Signal,
        _ = terminate.recv() => Exit::Signal,
        result = &mut server => Exit::Server(result.unwrap_or_else(|error| Err(error.into()))),
        error = watchdog_failure(&dog) => Exit::Watchdog(error),
    };
    backend.cloud.request_stop();
    stop_background.send_replace(true);
    // Keep health checks available until the watchdog has completed a normal stop.
    // A fatal failure releases the descriptor without disarming it.
    let dog_result = if matches!(exit, Exit::Signal) {
        tokio::task::spawn_blocking(move || dog.stop()).await?
    } else {
        drop(dog);
        Ok(())
    };
    stop_http.send_replace(true);
    let cloud = backend.cloud.clone();
    let cloud_result = tokio::task::spawn_blocking(move || cloud.stop()).await?;
    let _ = startup.await;
    let _ = reboot.await;
    dog_result?;
    cloud_result?;
    match exit {
        Exit::Signal => tokio::time::timeout(Duration::from_secs(6), server).await??,
        Exit::Server(Ok(())) => bail!("Core HTTP service stopped unexpectedly"),
        Exit::Server(Err(error)) => Err(error).context("Core HTTP service stopped"),
        Exit::Watchdog(error) => Err(anyhow!(error)),
    }
}

enum Exit {
    Signal,
    Server(Result<()>),
    Watchdog(String),
}

async fn watchdog_failure(dog: &watchdog::Manager) -> String {
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    loop {
        tick.tick().await;
        if let Some(error) = dog.failure() {
            return error;
        }
    }
}

async fn start_container(config: Config, mut stop: watch::Receiver<bool>) {
    if !config.container_enabled {
        return;
    }
    let timeout = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if *stop.borrow() {
            return;
        }
        let client = container::Client {
            socket: config.container_socket.clone(),
        };
        let probe = tokio::task::spawn_blocking(move || client.list());
        let ready = tokio::select! {
            _ = stop.changed() => return,
            _ = tokio::time::sleep_until(timeout) => {
                eprintln!("naos-core: container startup: wait for naosd timed out");
                return;
            }
            result = probe => result.is_ok_and(|result| result.is_ok()),
        };
        if ready {
            break;
        }
        tokio::select! {
            _ = stop.changed() => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
    if !*stop.borrow() {
        match tokio::task::spawn_blocking(move || {
            let client = container::Client {
                socket: config.container_socket,
            };
            client.start_manifest(&config.container_manifest)?;
            Ok::<String, container::Error>(config.container_manifest)
        })
        .await
        {
            Ok(Ok(path)) => eprintln!("naos-core: container started from {path}"),
            Ok(Err(error)) if !*stop.borrow() => eprintln!("naos-core: container startup: {error}"),
            _ => {}
        }
    }
}

async fn auto_reboot(updates: Arc<update::Client>, mut stop: watch::Receiver<bool>) {
    let mut attempted = String::new();
    let mut previous_error = String::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.tick().await;
    loop {
        tokio::select! {
            _ = stop.changed() => return,
            _ = tick.tick() => {}
        }
        let updates = updates.clone();
        let id = attempted.clone();
        let request = tokio::task::spawn_blocking(move || updates.prepare_auto_reboot(&id)).await;
        if *stop.borrow() {
            return;
        }
        match request {
            Ok(Ok(Some((task, lease)))) => {
                attempted = task.id.clone();
                previous_error.clear();
                eprintln!(
                    "naos-core: update task {} ({}, version {}) installed; rebooting into {}",
                    task.id, task.source, task.version, task.target_slot
                );
                match run_reboot(lease).await {
                    Ok(_keep) => {
                        let _ = stop.wait_for(|stopped| *stopped).await;
                        return;
                    }
                    Err(error) => eprintln!(
                        "naos-core: automatic reboot for update task {} failed: {error}",
                        task.id
                    ),
                }
            }
            Ok(Ok(None)) => previous_error.clear(),
            Ok(Err(error)) => {
                let message = error.to_string();
                if message != previous_error {
                    eprintln!("naos-core: automatic update reboot deferred: {message}");
                    previous_error = message;
                }
            }
            Err(error) => {
                eprintln!("naos-core: automatic reboot worker: {error}");
                return;
            }
        }
    }
}

async fn run_reboot(lease: Lease) -> Result<Lease> {
    tokio::task::spawn_blocking(move || {
        device::reboot()?;
        Ok(lease)
    })
    .await?
}
