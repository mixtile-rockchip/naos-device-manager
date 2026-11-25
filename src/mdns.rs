use crate::device;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PREFIX: &str = "naos-core-";
const SUFFIX: &str = ".service";
const MAX_SERVICES: usize = 32;
const MAX_XML: u64 = 16 * 1024;

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("invalid mDNS service: {0}")]
    Invalid(&'static str),
    #[error("mDNS service not found")]
    NotFound,
    #[error("mDNS service limit reached: at most 32 registrations are allowed")]
    Conflict,
    #[error("Avahi service files unavailable: {0}")]
    Unavailable(String),
}

fn unavailable(error: impl std::fmt::Display) -> Error {
    Error::Unavailable(error.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub txt: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct Registration {
    pub id: String,
    #[serde(flatten)]
    pub service: Service,
    pub state: &'static str,
}

#[derive(Deserialize, Serialize)]
#[serde(rename = "service-group")]
struct Declaration {
    name: String,
    #[serde(rename = "service")]
    services: Vec<DeclarationService>,
}

#[derive(Deserialize, Serialize)]
struct DeclarationService {
    #[serde(rename = "type")]
    kind: String,
    #[serde(
        rename = "host-name",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    host: String,
    port: u16,
    #[serde(rename = "txt-record", default, skip_serializing_if = "Vec::is_empty")]
    txt: Vec<String>,
}

pub struct Manager {
    directory: PathBuf,
    executable: PathBuf,
    hostname: fn() -> io::Result<String>,
    access: Mutex<()>,
}

impl Default for Manager {
    fn default() -> Self {
        Self {
            directory: "/etc/avahi/services".into(),
            executable: "/usr/sbin/avahi-daemon".into(),
            hostname: device::hostname,
            access: Mutex::new(()),
        }
    }
}

impl Manager {
    fn ready(&self) -> Result<(), Error> {
        let info = fs::metadata(&self.executable).map_err(unavailable)?;
        if !info.is_file() || info.permissions().mode() & 0o111 == 0 {
            return Err(unavailable("avahi-daemon is not executable"));
        }
        if !fs::metadata(&self.directory).map_err(unavailable)?.is_dir() {
            return Err(unavailable("service path is not a directory"));
        }
        Ok(())
    }

    pub fn put(&self, id: &str, mut service: Service) -> Result<Registration, Error> {
        validate(id, &mut service)?;
        let _access = self.access.lock().expect("Avahi service store poisoned");
        self.ready()?;
        let host = canonical_host(&(self.hostname)().map_err(unavailable)?)?;
        if !service.host.is_empty()
            && !service
                .host
                .trim_end_matches('.')
                .eq_ignore_ascii_case(&host)
        {
            return Err(Error::Invalid("host must be the device hostname"));
        }
        service.host = host;
        let path = self.path(id);
        let old = match fs::symlink_metadata(&path) {
            Ok(info) => {
                let mut stored = read(&path, id)?.service;
                if stored.host.is_empty()
                    || stored
                        .host
                        .trim_end_matches('.')
                        .eq_ignore_ascii_case(&service.host)
                {
                    stored.host.clone_from(&service.host);
                }
                if stored == service {
                    return Ok(registration(id, service));
                }
                Some(info)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if self.managed_files()?.len() >= MAX_SERVICES {
                    return Err(Error::Conflict);
                }
                None
            }
            Err(error) => return Err(unavailable(error)),
        };
        let declaration = Declaration {
            name: service.name.clone(),
            services: vec![DeclarationService {
                kind: service.kind.clone(),
                host: service.host.clone(),
                port: service.port,
                txt: service
                    .txt
                    .iter()
                    .map(|(key, val)| format!("{key}={val}"))
                    .collect(),
            }],
        };
        let mut xml = String::from(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE service-group SYSTEM \"avahi-service.dtd\">\n",
        );
        let mut serializer = quick_xml::se::Serializer::new(&mut xml);
        serializer.indent(' ', 2);
        declaration.serialize(serializer).map_err(unavailable)?;
        xml.push('\n');
        let mut file = tempfile::Builder::new()
            .prefix(".naos-core-")
            .tempfile_in(&self.directory)
            .map_err(unavailable)?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o644))
            .map_err(unavailable)?;
        file.write_all(xml.as_bytes()).map_err(unavailable)?;
        // Avahi observes mtime at one-second resolution.
        let mut mtime = SystemTime::now();
        if let Some(info) = old
            && let Ok(prior) = info.modified()
            && seconds(prior) >= seconds(mtime)
        {
            mtime = UNIX_EPOCH + Duration::from_secs(seconds(prior) + 1);
        }
        file.as_file()
            .set_times(FileTimes::new().set_modified(mtime).set_accessed(mtime))
            .map_err(unavailable)?;
        file.as_file().sync_all().map_err(unavailable)?;
        file.persist(path).map_err(unavailable)?;
        File::open(&self.directory)
            .and_then(|dir| dir.sync_all())
            .map_err(unavailable)?;
        Ok(registration(id, service))
    }

    pub fn list(&self) -> Result<Vec<Registration>, Error> {
        let _access = self.access.lock().expect("Avahi service store poisoned");
        self.ready()?;
        self.managed_files()?
            .iter()
            .map(|id| read(&self.path(id), id))
            .collect()
    }

    pub fn delete(&self, id: &str) -> Result<(), Error> {
        if !valid_id(id) {
            return Err(Error::Invalid("invalid id"));
        }
        let _access = self.access.lock().expect("Avahi service store poisoned");
        self.ready()?;
        let path = self.path(id);
        match fs::symlink_metadata(&path) {
            Ok(info) if info.is_file() => {}
            Ok(_) => return Err(unavailable("managed service is not a regular file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(Error::NotFound),
            Err(error) => return Err(unavailable(error)),
        }
        fs::remove_file(path).map_err(unavailable)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{PREFIX}{id}{SUFFIX}"))
    }

    fn managed_files(&self) -> Result<Vec<String>, Error> {
        let mut result = Vec::new();
        for entry in fs::read_dir(&self.directory).map_err(unavailable)? {
            let name = entry.map_err(unavailable)?.file_name();
            if let Some(id) = name
                .to_str()
                .and_then(|s| s.strip_prefix(PREFIX)?.strip_suffix(SUFFIX))
                && valid_id(id)
            {
                result.push(id.to_owned());
            }
        }
        // The former implementation sorted the complete filename including its suffix.
        result.sort_by_key(|id| format!("{id}{SUFFIX}"));
        Ok(result)
    }
}

fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn registration(id: &str, service: Service) -> Registration {
    Registration {
        id: id.to_owned(),
        service,
        state: "configured",
    }
}

fn read(path: &Path, id: &str) -> Result<Registration, Error> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(unavailable)?;
    if !file.metadata().map_err(unavailable)?.is_file() {
        return Err(unavailable("managed service is not a regular file"));
    }
    let mut xml = String::new();
    file.take(MAX_XML + 1)
        .read_to_string(&mut xml)
        .map_err(unavailable)?;
    if xml.len() as u64 > MAX_XML {
        return Err(unavailable("service XML exceeds size limit"));
    }
    let mut doc: Declaration = quick_xml::de::from_str(&xml).map_err(unavailable)?;
    if doc.services.len() != 1 {
        return Err(unavailable("expected one service declaration"));
    }
    let d = doc.services.remove(0);
    let mut txt = BTreeMap::new();
    for record in d.txt {
        let (key, value) = record
            .split_once('=')
            .ok_or_else(|| unavailable("invalid stored TXT record"))?;
        if txt.insert(key.into(), value.into()).is_some() {
            return Err(unavailable("duplicate stored TXT key"));
        }
    }
    let mut service = Service {
        name: doc.name,
        kind: d.kind,
        port: d.port,
        host: d.host,
        txt,
    };
    validate(id, &mut service).map_err(unavailable)?;
    Ok(registration(id, service))
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .enumerate()
            .all(|(i, b)| b.is_ascii_alphanumeric() || (i > 0 && b".-_".contains(&b)))
}

fn valid_text(value: &str) -> bool {
    !value
        .chars()
        .any(|c| c.is_control() || c == '\u{fffe}' || c == '\u{ffff}')
}

fn validate(id: &str, service: &mut Service) -> Result<(), Error> {
    if !valid_id(id) {
        return Err(Error::Invalid("invalid id"));
    }
    if service.name.trim().is_empty() || service.name.len() > 63 || !valid_text(&service.name) {
        return Err(Error::Invalid(
            "name must contain 1-63 UTF-8 bytes without control characters",
        ));
    }
    service.kind.make_ascii_lowercase();
    let (label, protocol) = service
        .kind
        .strip_prefix('_')
        .and_then(|body| body.split_once('.'))
        .ok_or(Error::Invalid(
            "type must be _service._tcp or _service._udp",
        ))?;
    if !matches!(protocol, "_tcp" | "_udp")
        || !valid_label(label, 15)
        || !label.bytes().any(|byte| byte.is_ascii_alphabetic())
        || service.port == 0
    {
        return Err(Error::Invalid("invalid service type or port"));
    }
    if service.txt.len() > 32 {
        return Err(Error::Invalid("at most 32 TXT fields are allowed"));
    }
    let mut total = 0;
    let mut keys = HashSet::new();
    for (key, value) in &service.txt {
        if key.is_empty()
            || !key.bytes().all(|b| (0x20..=0x7e).contains(&b) && b != b'=')
            || !valid_text(value)
        {
            return Err(Error::Invalid("invalid TXT key or value"));
        }
        if !keys.insert(key.to_ascii_lowercase()) {
            return Err(Error::Invalid("TXT keys must be unique ignoring case"));
        }
        let size = key.len() + 1 + value.len();
        total += size + 1;
        if size > 255 || total > 1024 {
            return Err(Error::Invalid(
                "TXT item exceeds 255 bytes or total exceeds 1024 bytes",
            ));
        }
    }
    Ok(())
}

fn valid_label(label: &str, max: usize) -> bool {
    (1..=max).contains(&label.len())
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn canonical_host(host: &str) -> Result<String, Error> {
    let host = host.strip_suffix('.').unwrap_or(host);
    let qualified = if host.to_ascii_lowercase().ends_with(".local") {
        host.to_owned()
    } else {
        format!("{host}.local")
    };
    if qualified.len() > 253 || !qualified.split('.').all(|label| valid_label(label, 63)) {
        return Err(unavailable("invalid system hostname"));
    }
    Ok(qualified)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig() -> (tempfile::TempDir, Manager, Service) {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("avahi-daemon");
        File::create(&executable)
            .unwrap()
            .set_permissions(fs::Permissions::from_mode(0o700))
            .unwrap();
        let manager = Manager {
            directory: temp.path().into(),
            hostname: || Ok("NAOS".into()),
            executable,
            access: Mutex::new(()),
        };
        let service = Service {
            name: "Sensor <&>".into(),
            kind: "_Demo._TCP".into(),
            port: 443,
            host: String::new(),
            txt: BTreeMap::from([("path".into(), "/a&b".into())]),
        };
        (temp, manager, service)
    }

    #[test]
    fn service_round_trips_normalized_and_updates_mtime_only_on_change() {
        let (_temp, manager, mut service) = rig();
        let put = manager.put("demo", service.clone()).unwrap();
        assert_eq!(put.service.host, "NAOS.local");
        assert_eq!(put.service.kind, "_demo._tcp");
        assert_eq!(manager.list().unwrap()[0].service, put.service);
        let path = manager.path("demo");
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        manager.put("demo", service.clone()).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        service.port += 1;
        manager.put("demo", service).unwrap();
        assert!(seconds(fs::metadata(path).unwrap().modified().unwrap()) > seconds(mtime));
        manager.delete("demo").unwrap();
        assert!(manager.list().unwrap().is_empty());
        assert_eq!(manager.delete("demo"), Err(Error::NotFound));
    }

    #[test]
    fn rejects_alias_txt_collision_and_symlink() {
        let (_temp, manager, mut service) = rig();
        service.host = "other.local".into();
        assert!(matches!(
            manager.put("demo", service.clone()),
            Err(Error::Invalid(_))
        ));
        service.host.clear();
        service.txt.insert("PATH".into(), "duplicate".into());
        assert!(matches!(
            manager.put("demo", service.clone()),
            Err(Error::Invalid(_))
        ));
        service.txt.remove("PATH");
        std::os::unix::fs::symlink(&manager.executable, manager.path("demo")).unwrap();
        assert!(matches!(
            manager.put("demo", service),
            Err(Error::Unavailable(_))
        ));
        assert!(matches!(manager.delete("demo"), Err(Error::Unavailable(_))));
        assert!(manager.list().is_err());
    }
}
