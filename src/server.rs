use crate::api::Transport;
use anyhow::{Context, Result, bail, ensure};
use axum::Router;
use axum::extract::Extension;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use std::fs::{self, Metadata};
use std::io::{self, Read};
use std::net::SocketAddr;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

const MAX_CONNECTIONS_PER_LISTENER: usize = 128;

pub struct Bound {
    tcp: Option<TcpListener>,
    unix: UnixListener,
    _socket_path: OwnedSocket,
}

struct OwnedSocket {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl OwnedSocket {
    fn new(path: PathBuf, info: &Metadata) -> Self {
        Self {
            path,
            device: info.dev(),
            inode: info.ino(),
        }
    }

    fn still_owned(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|info| {
            info.file_type().is_socket() && (info.dev(), info.ino()) == (self.device, self.inode)
        })
    }
}

impl Drop for OwnedSocket {
    fn drop(&mut self) {
        if self.still_owned() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl Bound {
    pub async fn bind(path: &Path, http: Option<&str>) -> Result<Self> {
        ensure!(path.is_absolute(), "core.socket must be an absolute path");
        let tcp = if let Some(http) = http {
            Some(TcpListener::bind(loopback_address(http)?).await?)
        } else {
            None
        };
        fs::create_dir_all(
            path.parent()
                .context("core.socket must have a parent directory")?,
        )?;
        remove_stale(path).await?;
        let unix = UnixListener::bind(path)?;
        let socket_path = OwnedSocket::new(path.to_owned(), &fs::symlink_metadata(path)?);
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            tcp,
            unix,
            _socket_path: socket_path,
        })
    }

    pub async fn run(&self, app: Router, stop: watch::Receiver<bool>) -> Result<()> {
        self.run_with_limit(app, stop, MAX_CONNECTIONS_PER_LISTENER)
            .await
    }

    async fn run_with_limit(
        &self,
        app: Router,
        mut stop: watch::Receiver<bool>,
        limit: usize,
    ) -> Result<()> {
        let unix = app.clone().layer(Extension(Transport::Unix));
        let tcp = app.layer(Extension(Transport::Tcp));
        let mut unix_connections = JoinSet::new();
        let mut tcp_connections = JoinSet::new();
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                _ = stop.changed() => break,
                _ = unix_connections.join_next(), if !unix_connections.is_empty() => {}
                _ = tcp_connections.join_next(), if !tcp_connections.is_empty() => {}
                // TCP clients must leave capacity for local control and watchdog health.
                result = self.unix.accept(), if unix_connections.len() < limit => {
                    let (stream, _) = result?;
                    unix_connections.spawn(connection(stream, unix.clone(), stop.clone()));
                }
                result = async {
                    match &self.tcp {
                        Some(listener) => listener.accept().await,
                        None => std::future::pending().await,
                    }
                }, if tcp_connections.len() < limit => {
                    let (stream, _) = result?;
                    tcp_connections.spawn(connection(stream, tcp.clone(), stop.clone()));
                }
            }
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            while unix_connections.join_next().await.is_some() {}
            while tcp_connections.join_next().await.is_some() {}
        })
        .await;
        Ok(())
    }
}

fn loopback_address(value: &str) -> Result<SocketAddr> {
    let address: SocketAddr = value
        .parse()
        .context("expected a numeric loopback address and port")?;
    ensure!(
        address.ip().to_canonical().is_loopback() && address.port() != 0,
        "http.listen must use a numeric loopback address and nonzero port"
    );
    Ok(address)
}

async fn remove_stale(path: &Path) -> Result<()> {
    let info = match fs::symlink_metadata(path) {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        info.file_type().is_socket(),
        "core socket path already exists and is not a socket: {}",
        path.display()
    );
    let probe = tokio::time::timeout(Duration::from_secs(1), UnixStream::connect(path)).await?;
    match probe {
        Ok(_) => bail!("another Core server is using the Unix socket"),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) => {}
        Err(error) => return Err(error.into()),
    }
    let current = match fs::symlink_metadata(path) {
        Ok(info) => info,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        current.file_type().is_socket()
            && (current.dev(), current.ino()) == (info.dev(), info.ino()),
        "core socket path changed during probe"
    );
    fs::remove_file(path)?;
    Ok(())
}

async fn connection<I>(stream: I, app: Router, mut stop: watch::Receiver<bool>)
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut builder = http1::Builder::new();
    builder
        .timer(TokioTimer::new())
        .header_read_timeout(Duration::from_secs(5))
        .max_buf_size(8192);
    let connection = builder.serve_connection(TokioIo::new(stream), TowerToHyperService::new(app));
    tokio::pin!(connection);
    tokio::select! {
        _ = &mut connection => {},
        _ = async { let _ = stop.wait_for(|stopped| *stopped).await; } => {
            connection.as_mut().graceful_shutdown();
            let _ = tokio::time::timeout(Duration::from_secs(5), &mut connection).await;
        }
    }
}

pub struct UnixClient {
    client: reqwest::blocking::Client,
}

impl UnixClient {
    pub fn new(path: &Path, timeout: Duration) -> Result<Self, reqwest::Error> {
        let client = reqwest::blocking::Client::builder()
            .unix_socket(path.to_path_buf())
            .tls_built_in_root_certs(false)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(1))
            .timeout(timeout)
            .build()?;
        Ok(Self { client })
    }

    pub fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        data: Option<&serde_json::Value>,
    ) -> Result<reqwest::blocking::Response, reqwest::Error> {
        let post = method == reqwest::Method::POST;
        let mut request = self
            .client
            .request(method, format!("http://localhost{path}"));
        if let Some(data) = data {
            request = request.json(data);
        } else if post {
            request = request.header(reqwest::header::CONTENT_TYPE, "application/json");
        }
        request.send()
    }
}

pub fn read_response(response: reqwest::blocking::Response, maximum: u64) -> Result<Vec<u8>> {
    let mut data = Vec::new();
    response.take(maximum + 1).read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= maximum, "cannot read bounded response");
    Ok(data)
}

pub fn socket_unavailable(error: &reqwest::Error) -> bool {
    use std::error::Error as _;
    let mut source = error.source();
    while let Some(error) = source {
        if let Some(io) = error.downcast_ref::<io::Error>()
            && matches!(
                io.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            )
        {
            return true;
        }
        source = error.source();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    #[test]
    fn loopback_validation_matches_config_for_ipv4_mapped_ipv6() {
        for value in ["[::ffff:127.0.0.1]:8890", "127.0.0.1:8890", "[::1]:8890"] {
            assert!(loopback_address(value).is_ok(), "{value}");
        }
        for value in [
            "[::ffff:192.0.2.1]:8890",
            "[::]:8890",
            "[::1]:0",
            "localhost:8890",
        ] {
            assert!(loopback_address(value).is_err(), "{value}");
        }
    }

    #[tokio::test]
    async fn unix_listener_has_private_permissions_and_rejects_a_second_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core.sock");
        fs::write(&path, b"keep").unwrap();
        assert!(Bound::bind(&path, None).await.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"keep");
        fs::remove_file(&path).unwrap();
        assert!(Bound::bind(&path, Some("0.0.0.0:8890")).await.is_err());

        let stale = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(stale);
        let server = Bound::bind(&path, None).await.unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(Bound::bind(&path, None).await.is_err());
        drop(server);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn real_unix_http_server_exposes_transport_and_shuts_down() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("core.sock");
        let bound = Bound::bind(&path, None).await.unwrap();
        let app = Router::new().route(
            "/healthz",
            get(|Extension(kind): Extension<Transport>| async move {
                assert_eq!(kind, Transport::Unix);
                "healthy"
            }),
        );
        let (stop, receiver) = watch::channel(false);
        let running = tokio::spawn(async move { bound.run(app, receiver).await.unwrap() });
        let mut stream = UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("healthy"));
        stop.send(true).unwrap();
        running.await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn unix_health_remains_available_when_tcp_connections_are_full() {
        const LIMIT: usize = 2;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("core.sock");
        let mut bound = Bound::bind(&path, None).await.unwrap();
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        bound.tcp = Some(tcp);

        let (started, mut started_requests) = tokio::sync::mpsc::channel(LIMIT);
        let (release, release_requests) = watch::channel(false);
        let app = Router::new()
            .route(
                "/hold",
                get(move |Extension(kind): Extension<Transport>| {
                    let started = started.clone();
                    let mut release_requests = release_requests.clone();
                    async move {
                        assert_eq!(kind, Transport::Tcp);
                        started.send(()).await.unwrap();
                        release_requests.wait_for(|ready| *ready).await.unwrap();
                        "done"
                    }
                }),
            )
            .route(
                "/healthz",
                get(|Extension(kind): Extension<Transport>| async move {
                    assert_eq!(kind, Transport::Unix);
                    "healthy"
                }),
            );
        let (stop, receiver) = watch::channel(false);
        let running =
            tokio::spawn(async move { bound.run_with_limit(app, receiver, LIMIT).await.unwrap() });

        let mut busy = Vec::new();
        for _ in 0..LIMIT {
            let mut tcp = TcpStream::connect(address).await.unwrap();
            tcp.write_all(b"GET /hold HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            busy.push(tcp);
        }
        for _ in 0..LIMIT {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), started_requests.recv())
                    .await
                    .unwrap(),
                Some(())
            );
        }
        let mut unix = UnixStream::connect(&path).await.unwrap();
        unix.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), unix.read_to_string(&mut response))
            .await
            .expect("TCP capacity blocked local health")
            .unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("healthy"));

        release.send(true).unwrap();
        stop.send(true).unwrap();
        for mut tcp in busy {
            let mut reply = String::new();
            tokio::time::timeout(Duration::from_secs(2), tcp.read_to_string(&mut reply))
                .await
                .unwrap()
                .unwrap();
            assert!(reply.ends_with("done"));
        }
        running.await.unwrap();
        assert!(!path.exists());
    }
}
