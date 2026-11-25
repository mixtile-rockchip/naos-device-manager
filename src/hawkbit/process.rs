use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::time::Duration;

use anyhow::{Result, anyhow, ensure};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub(super) const RUNTIME_DIRECTORY: &str = "/run/naos/core";
const UPDATER_BINARY: &str = "/usr/bin/rauc-hawkbit-updater";
const LOG_LIMIT: u64 = 64 * 1024;
const LOG_WINDOW: u64 = 4096;

#[derive(Clone, Debug, Serialize)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub start_time: u64,
    pub boot_id: String,
    pub exe: String,
    pub config_path: String,
    pub config_sha256: String,
}

#[derive(Clone)]
pub(super) struct Options {
    pub(super) directory: PathBuf,
    pub(super) updater: PathBuf,
    pub(super) version_file: PathBuf,
    pub(super) os_release: PathBuf,
    pub(super) monitor_interval: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            directory: RUNTIME_DIRECTORY.into(),
            updater: UPDATER_BINARY.into(),
            version_file: "/usr/lib/naos/version.json".into(),
            os_release: "/etc/os-release".into(),
            monitor_interval: Duration::from_millis(500),
        }
    }
}

impl Options {
    pub(super) fn config_path(&self) -> PathBuf {
        self.directory.join("hawkbit.conf")
    }

    pub(super) fn log_path(&self) -> PathBuf {
        self.directory.join("hawkbit-updater.log")
    }
}

fn private_file(metadata: &Metadata, check_mode: bool) -> bool {
    // SAFETY: geteuid has no memory or resource ownership requirements.
    let owner = unsafe { libc::geteuid() };
    metadata.is_file()
        && metadata.nlink() == 1
        && metadata.uid() == owner
        && (!check_mode || metadata.mode() & 0o077 == 0)
}

fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        private_file(&metadata, true) && metadata.len() <= limit,
        "invalid private hawkBit file"
    );
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= limit,
        "private hawkBit file exceeds limit"
    );
    Ok(data)
}

fn process_start_time(data: &[u8]) -> Result<u64> {
    let end = data
        .windows(2)
        .rposition(|window| window == b") ")
        .ok_or_else(|| anyhow!("invalid Linux process stat"))?;
    let mut fields = data[end + 2..]
        .split(|byte| byte.is_ascii_whitespace())
        .filter(|field| !field.is_empty());
    match fields.next() {
        Some(b"X" | b"Z") => return Err(io::Error::from(io::ErrorKind::NotFound).into()),
        Some(_) => {}
        None => return Err(anyhow!("invalid Linux process stat")),
    }
    fields
        .nth(18)
        .and_then(|field| std::str::from_utf8(field).ok())
        .and_then(|field| field.parse().ok())
        .ok_or_else(|| anyhow!("invalid process start time"))
}

pub(super) fn process_identity(
    pid: u32,
    config_path: &Path,
    data: &[u8],
) -> Result<ProcessIdentity> {
    let pid = i32::try_from(pid)?;
    ensure!(pid > 0, "invalid process identifier");
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let start_time = process_start_time(&fs::read(proc.join("stat"))?)?;
    let exe = fs::read_link(proc.join("exe"))?
        .to_string_lossy()
        .into_owned();
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned();
    Ok(ProcessIdentity {
        pid,
        start_time,
        boot_id,
        exe,
        config_path: config_path.to_string_lossy().into_owned(),
        config_sha256: format!("{:x}", Sha256::digest(data)),
    })
}

pub fn has_running_updater() -> Result<bool> {
    has_running_executable(Path::new(UPDATER_BINARY))
}

pub(super) fn has_running_executable(executable: &Path) -> Result<bool> {
    let executable = executable
        .canonicalize()
        .unwrap_or_else(|_| executable.to_path_buf());
    let entries =
        fs::read_dir("/proc").map_err(|_| anyhow!("cannot inspect running updater processes"))?;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|value| value.parse::<u32>().ok()) else {
            continue;
        };
        if pid == process::id() {
            continue;
        }
        if let Ok(path) = fs::read_link(entry.path().join("exe")) {
            let path = path.as_os_str().as_encoded_bytes();
            let path = path.strip_suffix(b" (deleted)").unwrap_or(path);
            if path == executable.as_os_str().as_encoded_bytes() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn private_dir(path: &Path) -> Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|_| anyhow!("cannot create private hawkBit directory"))?;
    let metadata = path
        .symlink_metadata()
        .map_err(|_| anyhow!("cannot access private hawkBit directory"))?;
    ensure!(metadata.is_dir(), "hawkBit directory must not be a symlink");
    ensure!(
        metadata.mode() & 0o077 == 0,
        "hawkBit directory must already be private (0700)"
    );
    Ok(())
}

pub(super) fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("invalid private hawkBit file path"))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".hawkbit-")
        .tempfile_in(parent)
        .map_err(|_| anyhow!("cannot create private hawkBit file"))?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| anyhow!("cannot protect hawkBit file"))?;
    temp.write_all(data)
        .and_then(|_| temp.as_file().sync_all())
        .map_err(|_| anyhow!("cannot write or sync hawkBit file"))?;
    temp.persist(path)
        .map_err(|_| anyhow!("cannot publish hawkBit file"))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| anyhow!("cannot sync hawkBit directory"))?;
    Ok(())
}

pub(super) fn open_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| anyhow!("cannot open hawkBit process lock"))?;
    ensure!(
        private_file(&file.metadata()?, false),
        "invalid hawkBit process lock"
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    // SAFETY: The file owns a valid descriptor for the lifetime of the lock.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    ensure!(result == 0, "another hawkBit controller is active");
    Ok(file)
}

pub(super) fn open_log(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| anyhow!("cannot open updater log"))?;
    ensure!(
        private_file(&file.metadata()?, false),
        "invalid updater log ownership or file type"
    );
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

pub(super) fn rotate_log(file: &File, path: &Path, fresh_run: bool) -> Result<()> {
    let len = file.metadata()?.len();
    if len == 0 || !fresh_run && len <= LOG_LIMIT {
        return Ok(());
    }
    let tail_len = len.min(LOG_LIMIT);
    let mut tail = vec![0; tail_len as usize];
    let bytes_read = file.read_at(&mut tail, len - tail_len)?;
    let mut archive = path.as_os_str().to_os_string();
    archive.push(".1");
    atomic_write(Path::new(&archive), &tail[..bytes_read])?;
    file.set_len(0)?;
    Ok(())
}

pub(super) fn recent_logs(options: &Options, active_token: &str) -> Vec<String> {
    let read = || -> Result<(String, u64)> {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(options.log_path())?;
        let metadata = file.metadata()?;
        ensure!(private_file(&metadata, true), "invalid private updater log");
        let start = metadata.len().saturating_sub(LOG_WINDOW);
        file.seek(SeekFrom::Start(start))?;
        let mut data = Vec::new();
        file.take(LOG_WINDOW).read_to_end(&mut data)?;
        let text = String::from_utf8_lossy(&data).into_owned();
        Ok((text, start))
    };
    let Ok((mut text, start)) = read() else {
        return Vec::new();
    };
    if start > 0 {
        let Some((_, rest)) = text.split_once('\n') else {
            return Vec::new();
        };
        text = rest.to_owned();
    }
    if !active_token.is_empty() {
        text = text.replace(active_token, "[redacted]");
    }
    if let Ok(data) = read_private(&options.config_path(), 8192)
        && let Ok(ini) = std::str::from_utf8(&data)
    {
        for line in ini.lines() {
            if let Some(("auth_token" | "gateway_token", token)) = line.split_once('=')
                && !token.is_empty()
            {
                text = text.replace(token, "[redacted]");
            }
        }
    }
    let mut lines: Vec<_> = text
        .trim()
        .lines()
        .rev()
        .take(16)
        .map(|line| {
            let mut sanitized: String = line.chars().filter(|&c| c >= ' ' || c == '\t').collect();
            if sanitized.len() > 256 {
                sanitized.truncate(sanitized.floor_char_boundary(256));
            }
            sanitized
        })
        .collect();
    lines.reverse();
    lines
}

pub(super) fn random_request_id() -> Result<String> {
    let mut nonce = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut nonce)?;
    let hex = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(format!("hawkbit-{hex}"))
}

pub(super) fn updater_command(options: &Options, output: &File) -> Result<Command> {
    let mut command = Command::new(&options.updater);
    command
        .arg("--config-file")
        .arg(options.config_path())
        .arg("--run-once")
        .stdout(output.try_clone()?)
        .stderr(output.try_clone()?);
    // SAFETY: setsid is async-signal-safe and changes only this child. This lets
    // the updater continue if Core receives a process-group shutdown signal.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(folder: &tempfile::TempDir) -> Options {
        Options {
            directory: folder.path().to_path_buf(),
            ..Options::default()
        }
    }

    #[test]
    fn parses_linux_stat_using_the_final_command_delimiter() {
        let data = format!("22 (weird) pro)c) S {} 12345 11", "2 ".repeat(18));
        assert_eq!(process_start_time(data.as_bytes()).unwrap(), 12345);
        assert!(process_start_time(data.replace(") S", ") Z").as_bytes()).is_err());
        assert!(process_start_time(b"22 (test) S 1 2").is_err());
    }

    #[test]
    fn reads_an_existing_linux_process_identity_and_sha256() {
        let identity =
            process_identity(process::id(), Path::new("/run/config"), b"configuration").unwrap();
        assert_eq!(identity.pid, process::id() as i32);
        assert!(identity.start_time > 0);
        assert!(identity.exe.starts_with('/'));
        assert!(!identity.boot_id.is_empty());
        assert_eq!(
            identity.config_sha256,
            format!("{:x}", Sha256::digest(b"configuration"))
        );
    }

    #[test]
    fn private_files_refuse_symlinks_hardlinks_and_shared_directories() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path().join("runtime");
        private_dir(&root).unwrap();
        assert_eq!(root.metadata().unwrap().mode() & 0o777, 0o700);
        let file = root.join("config");
        atomic_write(&file, b"secret").unwrap();
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        assert_eq!(read_private(&file, 6).unwrap(), b"secret");
        assert!(read_private(&file, 5).is_err());
        let symlink = root.join("symlink");
        std::os::unix::fs::symlink(&file, &symlink).unwrap();
        assert!(read_private(&symlink, 6).is_err());
        assert!(open_log(&symlink).is_err());
        assert!(open_lock(&symlink).is_err());
        fs::hard_link(&file, root.join("hardlink")).unwrap();
        assert!(read_private(&file, 6).is_err());
        assert!(open_log(&file).is_err());
        let shared = folder.path().join("shared");
        fs::create_dir(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(private_dir(&shared).is_err());
    }

    #[test]
    fn process_lock_is_exclusive_and_released_with_the_file() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("hawkbit.lock");
        let lock = open_lock(&path).unwrap();
        assert!(open_lock(&path).is_err());
        drop(lock);
        assert!(open_lock(&path).is_ok());
    }

    #[test]
    fn rotation_bounds_archive_preserves_appender_and_keeps_old_logs_private() {
        let folder = tempfile::tempdir().unwrap();
        let opts = options(&folder);
        let path = opts.log_path();
        let mut file = open_log(&path).unwrap();
        let mut appender = file.try_clone().unwrap();
        file.write_all(&vec![b'a'; LOG_LIMIT as usize + 1]).unwrap();
        rotate_log(&file, &path, false).unwrap();
        let archive = folder.path().join("hawkbit-updater.log.1");
        assert_eq!(
            read_private(&archive, LOG_LIMIT).unwrap().len() as u64,
            LOG_LIMIT
        );
        appender.write_all(b"after rotation\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"after rotation\n");
        rotate_log(&file, &path, true).unwrap();
        assert!(fs::read(&path).unwrap().is_empty());
        assert_eq!(
            read_private(&archive, LOG_LIMIT).unwrap(),
            b"after rotation\n"
        );
    }

    #[test]
    fn public_log_lines_are_redacted_bounded_and_sanitized() {
        let folder = tempfile::tempdir().unwrap();
        let opts = options(&folder);
        atomic_write(
            &opts.config_path(),
            b"auth_token=old-target\ngateway_token=old-gateway\n",
        )
        .unwrap();
        let mut file = open_log(&opts.log_path()).unwrap();
        for n in 1..=20 {
            writeln!(file, "line {n} old-target old-gateway active-token").unwrap();
        }
        writeln!(file, "escape\x1b{}", "岌".repeat(100)).unwrap();
        let lines = recent_logs(&opts, "active-token");
        assert_eq!(lines.len(), 16);
        assert_eq!(lines[0], "line 6 [redacted] [redacted] [redacted]");
        assert!(lines[15].starts_with("escape岌"));
        assert!(lines[15].len() <= 256);
    }
}
