use crate::container;
use crate::device::{OemError, OemInfo, RecoveryError, SystemInfo};
use crate::mdns;
use crate::operation::{Gate, Lease};
use crate::update;
use axum::body::{Body, to_bytes};
use axum::extract::{Extension, Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs;
use std::io;
use std::path::{Component, Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_JSON_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Transport {
    Unix,
    Tcp,
}

#[derive(Serialize, Debug)]
pub struct Reload {
    pub applied: bool,
    pub pending: bool,
}

pub trait Backend: Send + Sync + 'static {
    fn system(&self) -> anyhow::Result<SystemInfo>;
    fn oem(&self) -> Result<OemInfo, OemError>;
    fn containers(&self) -> Result<Vec<container::State>, container::Error>;
    fn container_change(&self, id: &str, action: &str) -> Result<(), container::Error>;
    fn tasks(&self) -> update::Result<Vec<update::Task>>;
    fn task(&self, id: &str) -> update::Result<update::Task>;
    fn install(&self, path: &str, id: &str) -> update::Result<update::Task>;
    fn update_status(&self) -> update::Result<update::Status>;
    fn update_busy(&self) -> update::Result<bool>;
    fn hawkbit(&self) -> Value;
    fn reload(&self, config: &FsPath) -> anyhow::Result<Reload>;
    fn mdns_put(&self, id: &str, service: mdns::Service)
    -> Result<mdns::Registration, mdns::Error>;
    fn mdns_list(&self) -> Result<Vec<mdns::Registration>, mdns::Error>;
    fn mdns_delete(&self, id: &str) -> Result<(), mdns::Error>;
    fn recovery(&self) -> Result<(), RecoveryError>;
    fn reboot(&self) -> anyhow::Result<()>;
}

#[derive(Default)]
struct Actions {
    restarting: Option<Lease>,
    recovering: bool,
}

#[derive(Clone)]
struct Api {
    backend: Arc<dyn Backend>,
    gate: Gate,
    actions: Arc<Mutex<Actions>>,
}

pub fn router(backend: Arc<dyn Backend>, gate: Gate) -> Router {
    let state = Api {
        backend,
        gate,
        actions: Arc::new(Mutex::new(Actions::default())),
    };
    Router::new()
        .route(
            "/healthz",
            get(|| async { success(json!({"status": "ok"})) }),
        )
        .route("/v1/system", get(system))
        .route("/v1/oem", get(oem))
        .route("/v1/containers", get(containers))
        .route("/v1/containers/{id}/{action}", post(container_change))
        .route("/v1/update", get(update_status).post(install))
        .route("/v1/update-tasks", get(tasks))
        .route("/v1/update-tasks/{id}", get(task))
        .route("/v1/reboot", post(reboot))
        .route("/v1/recovery", post(recovery))
        .route("/v1/hawkbit", get(hawkbit))
        .route("/v1/hawkbit/reload", post(reload))
        .route("/v1/mdns/services", get(mdns_list))
        .route("/v1/mdns/services/{id}", put(mdns_put).delete(mdns_delete))
        .with_state(state)
        .layer(middleware::from_fn(local_control))
}

async fn local_control(req: Request, next: Next) -> Response {
    if !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        && (req
            .headers()
            .get(header::ORIGIN)
            .is_some_and(|value| !value.is_empty())
            || req
                .headers()
                .get("sec-fetch-site")
                .is_some_and(|value| value == "cross-site"))
    {
        return failure(
            StatusCode::FORBIDDEN,
            "CROSS_ORIGIN",
            "cross-origin control requests are not allowed",
        );
    }
    next.run(req).await
}

fn respond(status: StatusCode, value: impl Serialize) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(value)).into_response()
}

fn success(value: impl Serialize) -> Response {
    respond(StatusCode::OK, value)
}

fn failure(status: StatusCode, code: &str, message: impl std::fmt::Display) -> Response {
    respond(
        status,
        json!({"error": {"code": code, "message": message.to_string()}}),
    )
}

fn invalid(code: &str, message: impl std::fmt::Display) -> Response {
    failure(StatusCode::BAD_REQUEST, code, message)
}

fn unavailable(code: &str, message: impl std::fmt::Display) -> Response {
    failure(StatusCode::SERVICE_UNAVAILABLE, code, message)
}

fn busy() -> Response {
    failure(
        StatusCode::CONFLICT,
        "UPDATE_BUSY",
        "another device operation is active",
    )
}

fn reboot_pending() -> Response {
    failure(
        StatusCode::CONFLICT,
        "REBOOT_PENDING",
        "device reboot is pending",
    )
}

async fn work(operation: impl FnOnce() -> Response + Send + 'static) -> Response {
    match tokio::task::spawn_blocking(operation).await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("naos-core: API worker: {error}");
            unavailable("SERVICE_UNAVAILABLE", "cannot complete device operation")
        }
    }
}

async fn read_json<T: DeserializeOwned>(headers: &HeaderMap, body: Body) -> anyhow::Result<T> {
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default();
    anyhow::ensure!(
        media_type.trim().eq_ignore_ascii_case("application/json"),
        "Content-Type must be application/json"
    );
    let data =
        tokio::time::timeout(Duration::from_secs(10), to_bytes(body, MAX_JSON_BYTES)).await??;
    anyhow::ensure!(
        data.iter().find(|&&value| !value.is_ascii_whitespace()) == Some(&b'{'),
        "expected one JSON object"
    );
    Ok(serde_json::from_slice(&data)?)
}

async fn system(State(api): State<Api>) -> Response {
    work(move || match api.backend.system() {
        Ok(info) => success(info),
        Err(_) => unavailable(
            "SYSTEM_INFO_UNAVAILABLE",
            "cannot read system version metadata",
        ),
    })
    .await
}

async fn oem(State(api): State<Api>) -> Response {
    work(move || match api.backend.oem() {
        Ok(info) => success(info),
        Err(OemError::Unsupported) => failure(
            StatusCode::NOT_IMPLEMENTED,
            "OEM_UNSUPPORTED",
            OemError::Unsupported,
        ),
    })
    .await
}

async fn containers(State(api): State<Api>) -> Response {
    work(move || match api.backend.containers() {
        Ok(containers) => success(json!({"containers": containers})),
        Err(error) => unavailable("CONTAINERS_UNAVAILABLE", error),
    })
    .await
}

async fn container_change(
    State(api): State<Api>,
    Path((id, action)): Path<(String, String)>,
) -> Response {
    if !matches!(action.as_str(), "start" | "stop" | "restart") {
        return failure(
            StatusCode::NOT_FOUND,
            "NOT_FOUND",
            "unknown container operation",
        );
    }
    if !container::valid_id(&id) {
        return invalid("INVALID_CONTAINER_ID", "invalid container ID");
    }
    work(move || {
        let actions = api.actions.lock().expect("control state poisoned");
        if actions.restarting.is_some() {
            return reboot_pending();
        }
        let Ok(_lease) = api.gate.try_acquire(&format!("container-{action}")) else {
            return busy();
        };
        drop(actions);
        match api.backend.container_change(&id, &action) {
            Ok(()) => success(json!({"status": "ok", "id": id, "action": action})),
            Err(error) => {
                let (status, code) = match &error {
                    container::Error::InvalidId | container::Error::Remote { code: 3, .. } => {
                        (StatusCode::BAD_REQUEST, "INVALID_CONTAINER_ID")
                    }
                    container::Error::Remote { code: 5, .. } => {
                        (StatusCode::NOT_FOUND, "CONTAINER_NOT_FOUND")
                    }
                    container::Error::Remote { code: 6 | 9, .. } => {
                        (StatusCode::CONFLICT, "CONTAINER_STATE_CONFLICT")
                    }
                    _ => (StatusCode::SERVICE_UNAVAILABLE, "CONTAINERS_UNAVAILABLE"),
                };
                failure(status, code, error)
            }
        }
    })
    .await
}

fn update_failure(error: update::Error) -> Response {
    let (status, code) = match &error {
        update::Error::Busy => (StatusCode::CONFLICT, "UPDATE_BUSY"),
        update::Error::Conflict => (StatusCode::CONFLICT, "REQUEST_CONFLICT"),
        update::Error::NotFound => (StatusCode::NOT_FOUND, "TASK_NOT_FOUND"),
        update::Error::Invalid(_) => (StatusCode::BAD_REQUEST, "INVALID_REQUEST"),
        _ => (StatusCode::SERVICE_UNAVAILABLE, "UPDATE_UNAVAILABLE"),
    };
    failure(status, code, error)
}

async fn tasks(State(api): State<Api>) -> Response {
    work(move || match api.backend.tasks() {
        Ok(tasks) => success(json!({"tasks": tasks})),
        Err(error) => update_failure(error),
    })
    .await
}

async fn task(State(api): State<Api>, Path(id): Path<String>) -> Response {
    work(move || match api.backend.task(&id) {
        Ok(task) => success(task),
        Err(error) => update_failure(error),
    })
    .await
}

async fn update_status(State(api): State<Api>) -> Response {
    work(move || match api.backend.update_status() {
        Ok(status) => success(status),
        Err(error) => unavailable("UPDATE_UNAVAILABLE", error),
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Install {
    #[serde(default)]
    path: String,
    #[serde(default)]
    request_id: String,
}

fn accepted_task(task: &update::Task) -> Response {
    respond(
        StatusCode::ACCEPTED,
        json!({
            "status": "accepted", "path": task.path, "task_id": task.id, "state": task.state
        }),
    )
}

async fn install(State(api): State<Api>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let request: Install = match read_json(&parts.headers, body).await {
        Ok(request) => request,
        Err(error) => return invalid("INVALID_REQUEST", error),
    };
    if !FsPath::new(&request.path).is_absolute() || request.path.contains('\0') {
        return invalid("INVALID_PATH", "path must be an absolute local file path");
    }
    let path = clean_absolute(FsPath::new(&request.path));
    work(move || {
        if !request.request_id.is_empty() {
            match api.backend.tasks() {
                Ok(tasks) => {
                    if let Some(task) = tasks
                        .iter()
                        .find(|task| task.request_id == request.request_id)
                    {
                        if task.source == "local" && task.path == path.to_string_lossy() {
                            return accepted_task(task);
                        }
                        return update_failure(update::Error::Conflict);
                    }
                }
                Err(error) => return update_failure(error),
            }
        }
        match fs::symlink_metadata(&path) {
            Ok(info) if info.is_file() => {}
            Ok(_) => return invalid("INVALID_PATH", "path must name a regular file"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return failure(StatusCode::NOT_FOUND, "INVALID_PATH", error);
            }
            Err(error) => return invalid("INVALID_PATH", error),
        }
        let actions = api.actions.lock().expect("control state poisoned");
        if actions.restarting.is_some() {
            return reboot_pending();
        }
        match api
            .backend
            .install(path.to_str().expect("path is UTF-8"), &request.request_id)
        {
            Ok(task) => accepted_task(&task),
            Err(error) => update_failure(error),
        }
    })
    .await
}

pub fn clean_absolute(path: &FsPath) -> PathBuf {
    let mut result = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::ParentDir => {
                result.pop();
            }
            Component::Normal(name) => result.push(name),
            _ => {}
        }
    }
    result
}

async fn hawkbit(State(api): State<Api>) -> Response {
    work(move || success(api.backend.hawkbit())).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigPath {
    #[serde(default)]
    config_path: String,
}

async fn reload(
    State(api): State<Api>,
    transport: Option<Extension<Transport>>,
    req: Request,
) -> Response {
    if transport.map(|Extension(value)| value) != Some(Transport::Unix) {
        return failure(
            StatusCode::FORBIDDEN,
            "UNIX_REQUIRED",
            "hawkBit reload requires the local Unix socket",
        );
    }
    let (parts, body) = req.into_parts();
    let Ok(ConfigPath { config_path }) = read_json(&parts.headers, body).await else {
        return invalid("INVALID_REQUEST", "invalid hawkBit reload request");
    };
    if !FsPath::new(&config_path).is_absolute() || config_path.contains('\0') {
        return invalid(
            "INVALID_PATH",
            "config_path must be an absolute local file path",
        );
    }
    work(move || {
        match api
            .backend
            .reload(&clean_absolute(FsPath::new(&config_path)))
        {
            Ok(reload) if reload.applied != reload.pending => respond(
                if reload.pending {
                    StatusCode::ACCEPTED
                } else {
                    StatusCode::OK
                },
                reload,
            ),
            _ => unavailable("APPLY_FAILED", "cannot apply hawkBit configuration"),
        }
    })
    .await
}

fn mdns_failure(error: mdns::Error) -> Response {
    let (status, code) = match &error {
        mdns::Error::Invalid(_) => (StatusCode::BAD_REQUEST, "INVALID_MDNS_SERVICE"),
        mdns::Error::NotFound => (StatusCode::NOT_FOUND, "MDNS_NOT_FOUND"),
        mdns::Error::Conflict => (StatusCode::CONFLICT, "MDNS_CONFLICT"),
        mdns::Error::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "MDNS_UNAVAILABLE"),
    };
    failure(status, code, error)
}

async fn mdns_put(State(api): State<Api>, Path(id): Path<String>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let service = match read_json(&parts.headers, body).await {
        Ok(service) => service,
        Err(error) => return invalid("INVALID_REQUEST", error),
    };
    work(move || match api.backend.mdns_put(&id, service) {
        Ok(registration) => success(registration),
        Err(error) => mdns_failure(error),
    })
    .await
}

async fn mdns_list(State(api): State<Api>) -> Response {
    work(move || match api.backend.mdns_list() {
        Ok(services) => success(json!({"services": services})),
        Err(error) => mdns_failure(error),
    })
    .await
}

async fn mdns_delete(State(api): State<Api>, Path(id): Path<String>) -> Response {
    work(move || match api.backend.mdns_delete(&id) {
        Ok(()) => (
            StatusCode::NO_CONTENT,
            [(header::CACHE_CONTROL, "no-store")],
        )
            .into_response(),
        Err(error) => mdns_failure(error),
    })
    .await
}

async fn reboot(State(api): State<Api>) -> Response {
    work(move || restart(api, false)).await
}

async fn recovery(State(api): State<Api>) -> Response {
    work(move || restart(api, true)).await
}

fn restart(api: Api, recover_data: bool) -> Response {
    let mut actions = api.actions.lock().expect("control state poisoned");
    if actions.restarting.is_some() {
        return reboot_pending();
    }
    let kind = if recover_data { "recovery" } else { "reboot" };
    let Ok(lease) = api.gate.try_acquire(kind) else {
        return busy();
    };
    match api.backend.update_busy() {
        Ok(false) => {}
        Ok(true) => {
            return failure(
                StatusCode::CONFLICT,
                "UPDATE_BUSY",
                "cannot restart while RAUC is installing",
            );
        }
        Err(error) => return unavailable("UPDATE_UNAVAILABLE", error),
    }
    if recover_data {
        if let Err(error) = api.backend.recovery() {
            if matches!(error, RecoveryError::Uncertain(_)) {
                actions.recovering = true;
                actions.restarting = Some(lease);
            }
            return unavailable("RECOVERY_FAILED", error);
        }
        actions.recovering = true;
    }
    actions.restarting = Some(lease);
    drop(actions);
    let launch = std::thread::Builder::new()
        .name("naos-restart".into())
        .spawn({
            let api = api.clone();
            move || {
                std::thread::sleep(Duration::from_millis(200));
                if let Err(error) = api.backend.reboot() {
                    eprintln!("naos-core: reboot failed: {error}");
                    let mut actions = api.actions.lock().expect("control state poisoned");
                    if !actions.recovering {
                        actions.restarting = None;
                    }
                }
            }
        });
    if let Err(error) = launch {
        let mut actions = api.actions.lock().expect("control state poisoned");
        if !actions.recovering {
            actions.restarting = None;
        }
        return unavailable("REBOOT_FAILED", error);
    }
    respond(StatusCode::ACCEPTED, json!({"status": "accepted"}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;

    struct Mock {
        bad_version: bool,
        uncertain_recovery: bool,
    }

    impl Backend for Mock {
        fn system(&self) -> anyhow::Result<SystemInfo> {
            anyhow::ensure!(!self.bad_version, "missing version");
            Ok(SystemInfo {
                version: "1.0".into(),
                ..Default::default()
            })
        }
        fn oem(&self) -> Result<OemInfo, OemError> {
            Err(OemError::Unsupported)
        }
        fn containers(&self) -> Result<Vec<container::State>, container::Error> {
            Ok(vec![])
        }
        fn container_change(&self, _: &str, _: &str) -> Result<(), container::Error> {
            Ok(())
        }
        fn tasks(&self) -> update::Result<Vec<update::Task>> {
            Ok(vec![])
        }
        fn task(&self, _: &str) -> update::Result<update::Task> {
            Err(update::Error::NotFound)
        }
        fn install(&self, _: &str, _: &str) -> update::Result<update::Task> {
            panic!("not expected")
        }
        fn update_status(&self) -> update::Result<update::Status> {
            panic!("not expected")
        }
        fn update_busy(&self) -> update::Result<bool> {
            Ok(false)
        }
        fn hawkbit(&self) -> Value {
            json!({"enabled": false, "state": "disabled"})
        }
        fn reload(&self, config: &FsPath) -> anyhow::Result<Reload> {
            assert_eq!(config, FsPath::new("/etc/naos-core/device.ini"));
            Ok(Reload {
                applied: false,
                pending: true,
            })
        }
        fn mdns_put(&self, _: &str, _: mdns::Service) -> Result<mdns::Registration, mdns::Error> {
            panic!("not expected")
        }
        fn mdns_list(&self) -> Result<Vec<mdns::Registration>, mdns::Error> {
            Ok(vec![])
        }
        fn mdns_delete(&self, _: &str) -> Result<(), mdns::Error> {
            Err(mdns::Error::NotFound)
        }
        fn recovery(&self) -> Result<(), RecoveryError> {
            if self.uncertain_recovery {
                return Err(RecoveryError::Uncertain(io::Error::other("test")));
            }
            Ok(())
        }
        fn reboot(&self) -> anyhow::Result<()> {
            anyhow::bail!("fake reboot")
        }
    }

    fn fake() -> Arc<Mock> {
        Arc::new(Mock {
            bad_version: false,
            uncertain_recovery: false,
        })
    }

    async fn call(
        api: Router,
        method: &str,
        path: &str,
        body: &str,
        origin: bool,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        if origin {
            request = request.header(header::ORIGIN, "https://invalid.example");
        }
        let response = api
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let status = response.status();
        let body = to_bytes(response.into_body(), MAX_JSON_BYTES)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    #[tokio::test]
    async fn wire_contract_for_firmware_tasks_oem_and_container_list() {
        let api = router(fake(), Gate::new());
        let (_, version) = call(api.clone(), "GET", "/v1/system", "", false).await;
        assert_eq!(
            version,
            json!({"version": "1.0", "build_id": "", "machine": ""})
        );
        let (status, result) = call(api.clone(), "GET", "/v1/oem", "", false).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(result["error"]["code"], "OEM_UNSUPPORTED");
        let (_, list) = call(api.clone(), "GET", "/v1/containers", "", false).await;
        assert_eq!(list, json!({"containers": []}));
        let (status, task) = call(api, "GET", "/v1/update-tasks/unknown", "", false).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(task["error"]["code"], "TASK_NOT_FOUND");
        let missing = router(
            Arc::new(Mock {
                bad_version: true,
                uncertain_recovery: false,
            }),
            Gate::new(),
        );
        let (status, error) = call(missing, "GET", "/v1/system", "", false).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error["error"]["code"], "SYSTEM_INFO_UNAVAILABLE");
    }

    #[tokio::test]
    async fn reload_requires_unix_and_rejects_unknown_fields_and_cross_origin() {
        let api = router(fake(), Gate::new());
        let body = r#"{"config_path":"/etc/naos-core/../naos-core/device.ini"}"#;
        let (status, error) = call(
            api.clone().layer(Extension(Transport::Tcp)),
            "POST",
            "/v1/hawkbit/reload",
            body,
            false,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(error["error"]["code"], "UNIX_REQUIRED");
        let local = api.layer(Extension(Transport::Unix));
        let (status, result) = call(local.clone(), "POST", "/v1/hawkbit/reload", body, false).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(result, json!({"applied":false, "pending":true}));
        let (status, error) = call(local.clone(), "POST", "/v1/hawkbit/reload", body, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(error["error"]["code"], "CROSS_ORIGIN");
        let (status, _) = call(
            local,
            "POST",
            "/v1/hawkbit/reload",
            r#"{"config_path":"/etc/file","token":"secret"}"#,
            false,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn update_is_bounded_and_requires_absolute_regular_bundle() {
        let api = router(fake(), Gate::new());
        for (body, code) in [
            (r#"{"path":"relative"}"#.to_owned(), "INVALID_PATH"),
            (r#"{"path":"/tmp"}"#.to_owned(), "INVALID_PATH"),
            (
                format!(r#"{{"path":"{}"}}"#, "a".repeat(MAX_JSON_BYTES)),
                "INVALID_REQUEST",
            ),
        ] {
            let (status, error) = call(api.clone(), "POST", "/v1/update", &body, false).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(error["error"]["code"], code);
        }
    }

    #[tokio::test]
    async fn uncertain_recovery_latches_operation_gate() {
        let gate = Gate::new();
        let api = router(
            Arc::new(Mock {
                bad_version: false,
                uncertain_recovery: true,
            }),
            gate.clone(),
        );
        let (status, error) = call(api.clone(), "POST", "/v1/recovery", "", false).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(error["error"]["code"], "RECOVERY_FAILED");
        assert_eq!(gate.active().as_deref(), Some("recovery"));
        let (status, error) = call(api, "POST", "/v1/containers/demo/start", "", false).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["error"]["code"], "REBOOT_PENDING");
    }
}
