use super::{ExternalIdentity, Result};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub(super) trait Environment: Send {
    fn process_state(&self, identity: &ExternalIdentity, boot_id: &str) -> (bool, bool);
    fn updater_present(&self) -> (bool, bool);
    fn boot_confirmed(&self) -> bool;
    fn running_version(&self) -> String;
}

pub(super) struct System;

impl Environment for System {
    fn process_state(&self, identity: &ExternalIdentity, boot: &str) -> (bool, bool) {
        if identity.boot_id != boot {
            return (false, true);
        }
        if identity.pid <= 0 || identity.start_time == 0 {
            return (false, false);
        }
        let directory = format!("/proc/{}", identity.pid);
        let data = match fs::read_to_string(Path::new(&directory).join("stat")) {
            Ok(data) => data,
            Err(err) => return (false, err.kind() == io::ErrorKind::NotFound),
        };
        let Some((state, start_time)) = start_stat(&data) else {
            return (false, false);
        };
        if start_time != identity.start_time || matches!(state, "Z" | "X") {
            return (false, true);
        }
        let exe = match fs::read_link(Path::new(&directory).join("exe")) {
            Ok(exe) => exe,
            Err(err) => return (false, err.kind() == io::ErrorKind::NotFound),
        };
        if exe.to_str().map(trim_deleted) != Some(identity.exe.as_str()) {
            return (false, false);
        }
        (true, true)
    }

    fn updater_present(&self) -> (bool, bool) {
        let Ok(entries) = fs::read_dir("/proc") else {
            return (false, false);
        };
        let mut complete = true;
        for entry in entries {
            let Ok(entry) = entry else {
                complete = false;
                continue;
            };
            if !entry
                .file_name()
                .to_str()
                .is_some_and(|s| s.parse::<i32>().is_ok())
            {
                continue;
            }
            match fs::read_link(entry.path().join("exe")) {
                Ok(exe)
                    if exe
                        .to_str()
                        .map(trim_deleted)
                        .is_some_and(|p| p.ends_with("/rauc-hawkbit-updater")) =>
                {
                    return (true, true);
                }
                Err(err) if err.kind() != io::ErrorKind::NotFound => complete = false,
                _ => {}
            }
        }
        (false, complete)
    }

    fn boot_confirmed(&self) -> bool {
        fs::metadata("/run/openrc/started/naos-boot-confirm").is_ok()
    }

    fn running_version(&self) -> String {
        #[derive(serde::Deserialize)]
        struct Version {
            version: String,
        }
        (|| {
            let mut data = Vec::new();
            File::open("/run/naos/lower/usr/lib/naos/version.json")
                .ok()?
                .take(65_537)
                .read_to_end(&mut data)
                .ok()?;
            if data.len() > 65_536 {
                return None;
            }
            serde_json::from_slice::<Version>(&data)
                .ok()
                .map(|v| v.version)
        })()
        .unwrap_or_default()
    }
}

fn trim_deleted(s: &str) -> &str {
    s.strip_suffix(" (deleted)").unwrap_or(s)
}

fn start_stat(stat: &str) -> Option<(&str, u64)> {
    let (_, trailing) = stat.rsplit_once(") ")?;
    let mut fields = trailing.split_ascii_whitespace();
    let state = fields.next()?;
    let start = fields.nth(18)?.parse().ok()?;
    Some((state, start))
}

pub(super) fn boot_id() -> Result<String> {
    let boot =
        fs::read_to_string("/proc/sys/kernel/random/boot_id").map_err(super::Error::unavailable)?;
    if !super::valid_id(boot.trim()) {
        return Err(super::Error::Unavailable(
            "Linux boot ID is unavailable".into(),
        ));
    }
    Ok(boot.trim().to_owned())
}

pub(super) fn config_digest(path: &str) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    let invalid = || {
        super::Error::Invalid("updater configuration is not a bounded private regular file".into())
    };
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| invalid())?;
    let info = file.metadata().map_err(|_| invalid())?;
    if !info.is_file() || info.permissions().mode() & 0o077 != 0 || info.len() > 65_536 {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(65_537)
        .read_to_end(&mut bytes)
        .map_err(|_| invalid())?;
    if bytes.len() > 65_536 {
        return Err(invalid());
    }
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::start_stat;

    #[test]
    fn reads_proc_start_time_after_parentheses_and_spaces_in_command() {
        let fields = [
            "S", "1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "11", "12", "13", "14", "15",
            "16", "17", "18", "991", "20",
        ];
        let stat = format!("17 (rauc) runner) {}", fields.join(" "));
        assert_eq!(start_stat(&stat), Some(("S", 991)));
        assert_eq!(start_stat("17 (rauc) S 1 2"), None);
    }
}
