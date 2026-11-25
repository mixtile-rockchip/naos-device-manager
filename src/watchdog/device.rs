use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, IntoRawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};

const GET_SUPPORT: u32 = 0x8028_5700;
const SET_OPTIONS: u32 = 0x8004_5704;
const KEEPALIVE: u32 = 0x8004_5705;
const SET_TIMEOUT: u32 = 0xc004_5706;
const GET_TIMEOUT: u32 = 0x8004_5707;
const DISABLE_CARD: i32 = 1;
const OPTION_SET_TIMEOUT: u32 = 0x0080;
const OPTION_MAGIC_CLOSE: u32 = 0x0100;
const OPTION_KEEPALIVE: u32 = 0x8000;

#[repr(C)]
#[derive(Default)]
struct WireInfo {
    options: u32,
    firmware: u32,
    identity: [u8; 32],
}

#[derive(Debug)]
pub struct Support {
    pub options: u32,
    pub firmware: u32,
    pub identity: String,
    pub can_set_timeout: bool,
    pub has_magic_close: bool,
    pub can_keepalive: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CloseResult {
    pub user_watchdog_disabled: bool,
    pub fd_closed: bool,
    pub magic_close_written: bool,
    // Disabling userspace ownership is not evidence of hardware reset.
    pub hardware_stopped_confirmed: bool,
}

pub struct Device {
    file: Option<File>,
}

impl Device {
    pub fn open(path: impl AsRef<Path>, expected_device: u64) -> Result<Self> {
        let path = path.as_ref();
        ensure!(
            path.is_absolute() && expected_device != 0,
            "expected an absolute device path and inspected device number"
        );
        let before = fs::symlink_metadata(path).context("stat watchdog")?;
        ensure!(
            before.file_type().is_char_device() && before.rdev() == expected_device,
            "watchdog path is not the inspected character device (symlinks refused)"
        );
        let file = OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .context("open watchdog")?;
        let matches = file.metadata().is_ok_and(|after| {
            after.file_type().is_char_device()
                && after.rdev() == expected_device
                && after.dev() == before.dev()
                && after.ino() == before.ino()
        });
        ensure!(
            matches,
            "post-open device identity changed; fd closed but watchdog may remain active"
        );
        Ok(Self { file: Some(file) })
    }

    fn file(&self) -> Result<&File> {
        self.file.as_ref().context("watchdog descriptor is closed")
    }

    fn ioctl<T>(&self, request: u32, argument: &mut T) -> Result<()> {
        let descriptor = self.file()?.as_raw_fd();
        // SAFETY: each call pairs the Linux watchdog UAPI request with its
        // matching writable C representation, alive for the duration of ioctl.
        let status = unsafe { libc::ioctl(descriptor, request as _, argument as *mut T) };
        if status < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(())
    }

    pub fn support(&self) -> Result<Support> {
        let mut info = WireInfo::default();
        self.ioctl(GET_SUPPORT, &mut info)?;
        let bytes = info
            .identity
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default();
        Ok(Support {
            options: info.options,
            firmware: info.firmware,
            identity: String::from_utf8_lossy(bytes).replace(char::REPLACEMENT_CHARACTER, "?"),
            can_set_timeout: info.options & OPTION_SET_TIMEOUT != 0,
            has_magic_close: info.options & OPTION_MAGIC_CLOSE != 0,
            can_keepalive: info.options & OPTION_KEEPALIVE != 0,
        })
    }

    pub fn timeout(&self) -> Result<i32> {
        let mut seconds = 0_i32;
        self.ioctl(GET_TIMEOUT, &mut seconds)?;
        ensure!(seconds > 0, "driver returned a non-positive timeout");
        Ok(seconds)
    }

    pub fn set_timeout(&self, mut seconds: i32) -> Result<i32> {
        ensure!(seconds > 0, "timeout must be positive C-int seconds");
        self.ioctl(SET_TIMEOUT, &mut seconds)?;
        self.timeout()
    }

    pub fn keep_alive(&self) -> Result<()> {
        self.ioctl(KEEPALIVE, &mut 0_i32)
    }

    pub fn stop_user_and_close(&mut self) -> Result<CloseResult> {
        let mut option = DISABLE_CARD;
        self.ioctl(SET_OPTIONS, &mut option)?;
        self.close_without_stopping()?;
        Ok(CloseResult {
            user_watchdog_disabled: true,
            fd_closed: true,
            ..CloseResult::default()
        })
    }

    pub fn magic_close(&mut self) -> Result<CloseResult> {
        let count = self.file()?.write(b"V")?;
        ensure!(
            count == 1,
            "short magic-close write; descriptor remains held"
        );
        self.close_without_stopping()?;
        Ok(CloseResult {
            fd_closed: true,
            magic_close_written: true,
            ..CloseResult::default()
        })
    }

    pub fn close_without_stopping(&mut self) -> Result<CloseResult> {
        let file = self.file.take().context("watchdog descriptor is closed")?;
        // SAFETY: into_raw_fd relinquishes File ownership. Linux close must
        // never be retried on a descriptor that the kernel may have reused.
        let status = unsafe { libc::close(file.into_raw_fd()) };
        if status < 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(CloseResult {
            fd_closed: true,
            ..CloseResult::default()
        })
    }

    pub(super) fn is_open(&self) -> bool {
        self.file.is_some()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Inspection {
    pub device: String,
    pub device_number: u64,
    pub sysfs_device: String,
    pub identity: String,
    pub driver: String,
    pub state: String,
    pub nowayout: bool,
    pub timeout_seconds: i32,
    pub reset_property_present: bool,
    pub hardware_stop_supported: Option<bool>,
}

pub fn inspect(device: &str) -> Result<Inspection> {
    inspect_with(device, Path::new("/sys/class/watchdog"), |path| {
        ensure!(path.is_absolute(), "expected an absolute device path");
        let info = fs::symlink_metadata(path)?;
        ensure!(
            info.file_type().is_char_device(),
            "expected an explicit character node, not a symlink"
        );
        Ok(info.rdev())
    })
}

fn inspect_with(
    device: &str,
    sysfs: &Path,
    validate: impl FnOnce(&Path) -> Result<u64>,
) -> Result<Inspection> {
    let path = Path::new(device);
    let basename = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("watchdog device name is required")?;
    ensure!(
        basename.strip_prefix("watchdog").is_some_and(
            |suffix| !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        ),
        "use an explicit numbered watchdog character node"
    );
    let number = validate(path)?;
    let node = sysfs.join(basename);
    let sysfs_device = read_text(&node.join("dev"))?;
    let major = ((number >> 8) & 0xfff) | ((number >> 32) & !0xfff);
    let minor = (number & 0xff) | ((number >> 12) & !0xff);
    ensure!(
        sysfs_device == format!("{major}:{minor}"),
        "device number does not match sysfs watchdog node"
    );
    let identity = read_text(&node.join("identity"))?;
    let state = read_text(&node.join("state"))?;
    let nowayout = match read_text(&node.join("nowayout"))?.as_str() {
        "0" => false,
        "1" => true,
        _ => bail!("unknown nowayout value"),
    };
    let timeout_seconds = read_text(&node.join("timeout"))?
        .parse::<i32>()
        .context("invalid sysfs timeout")?;
    ensure!(timeout_seconds > 0, "invalid sysfs timeout");
    let driver = fs::read_link(node.join("device/driver"))
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let reset_property_present = match fs::metadata(node.join("device/of_node/resets")) {
        Ok(metadata) => metadata.len() > 0,
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => return Err(error.into()),
    };
    let hardware_stop_supported = (driver == "dw_wdt").then_some(reset_property_present);
    Ok(Inspection {
        device: device.into(),
        device_number: number,
        sysfs_device,
        identity,
        driver,
        state,
        nowayout,
        timeout_seconds,
        reset_property_present,
        hardware_stop_supported,
    })
}

fn read_text(path: &Path) -> Result<String> {
    let mut buffer = Vec::new();
    File::open(path)?.take(4097).read_to_end(&mut buffer)?;
    ensure!(buffer.len() <= 4096, "sysfs attribute exceeds 4096 bytes");
    Ok(std::str::from_utf8(&buffer)?.trim().into())
}

pub fn validate_exercise(inspection: &Inspection) -> Result<()> {
    ensure!(
        inspection.state == "inactive",
        "refusing to take over a non-inactive watchdog"
    );
    ensure!(
        !inspection.nowayout,
        "short exercise requires confirmed nowayout=false"
    );
    ensure!(
        inspection.hardware_stop_supported == Some(true),
        "short exercise refused: no established hardware stop/reset path"
    );
    Ok(())
}

pub fn open_for_exercise(inspection: &Inspection) -> Result<Device> {
    validate_exercise(inspection)?;
    Device::open(&inspection.device, inspection.device_number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn sysfs() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let entry = directory.path().join("watchdog0");
        fs::create_dir_all(entry.join("device/of_node")).unwrap();
        for (file, value) in [
            ("dev", "240:3\n"),
            ("identity", "sbsa\n"),
            ("state", "inactive\n"),
            ("nowayout", "0\n"),
            ("timeout", "30\n"),
        ] {
            fs::write(entry.join(file), value).unwrap();
        }
        symlink("/drivers/dw_wdt", entry.join("device/driver")).unwrap();
        (directory, entry)
    }

    #[test]
    fn inspection_checks_device_and_reset_wiring_before_exercise() {
        let (directory, entry) = sysfs();
        let path = "/dev/watchdog0";
        let inspect = || inspect_with(path, directory.path(), |_| Ok((240 << 8) | 3));
        let report = inspect().unwrap();
        assert_eq!(report.identity, "sbsa");
        assert_eq!(report.timeout_seconds, 30);
        assert_eq!(report.hardware_stop_supported, Some(false));
        assert!(validate_exercise(&report).is_err());
        fs::write(entry.join("device/of_node/resets"), [0, 0, 0, 1]).unwrap();
        assert!(validate_exercise(&inspect().unwrap()).is_ok());
        fs::write(entry.join("nowayout"), b"1").unwrap();
        assert!(validate_exercise(&inspect().unwrap()).is_err());
        fs::write(entry.join("dev"), b"240:7").unwrap();
        assert!(inspect().is_err());
        assert!(
            inspect_with("/dev/watchdog", directory.path(), |_| panic!(
                "legacy alias opened"
            ))
            .is_err()
        );
    }

    #[test]
    fn rejects_files_and_symlinks_before_opening() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("watchdog0");
        fs::write(&path, b"data").unwrap();
        assert!(Device::open(&path, 1).is_err());
        let alias = directory.path().join("watchdog1");
        symlink(&path, &alias).unwrap();
        assert!(Device::open(&alias, 1).is_err());
        assert!(inspect(alias.to_str().unwrap()).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"data");
    }

    #[test]
    fn uapi_uses_linux_c_layout_and_keeps_magic_close_separate() {
        assert_eq!(std::mem::size_of::<WireInfo>(), 40);
        assert_eq!(std::mem::size_of::<libc::c_int>(), 4);
        assert_eq!(GET_SUPPORT, 0x8028_5700);
        assert_eq!(SET_TIMEOUT, 0xc004_5706);
        let mut device = Device {
            file: Some(tempfile::tempfile().unwrap()),
        };
        assert!(device.stop_user_and_close().is_err());
        assert!(device.is_open());
        let result = device.magic_close().unwrap();
        assert!(result.fd_closed);
        assert!(result.magic_close_written);
        assert!(!result.hardware_stopped_confirmed);
        assert!(device.close_without_stopping().is_err());
    }
}
