use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub const VERSION_FILE: &str = "/usr/lib/naos/version.json";
const OEM_OTP: &str = "/sys/bus/nvmem/devices/rockchip-secure-otp0/nvmem";
const MAX_METADATA: u64 = 64 * 1024;
const MISC: &str = "/dev/naos/misc";
const RECOVERY_OFFSET: u64 = 65536;
const RECOVERY_SIZE: usize = 512;

#[derive(Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemInfo {
    #[serde(deserialize_with = "string_or_null")]
    pub version: String,
    #[serde(deserialize_with = "string_or_null")]
    pub build_id: String,
    #[serde(deserialize_with = "string_or_null")]
    pub machine: String,
}

fn string_or_null<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Option::<String>::deserialize(deserializer).map(Option::unwrap_or_default)
}

pub fn system_info(path: &Path) -> Result<SystemInfo> {
    let input = fs::File::open(path)?;
    let mut data = Vec::new();
    input.take(MAX_METADATA + 1).read_to_end(&mut data)?;
    ensure!(
        data.len() as u64 <= MAX_METADATA,
        "invalid system version metadata"
    );
    let mut info: SystemInfo =
        serde_json::from_slice(&data).context("invalid system version metadata")?;
    for value in [&mut info.version, &mut info.build_id, &mut info.machine] {
        *value = value.trim().to_owned();
    }
    ensure!(!info.version.is_empty(), "system version is unavailable");
    Ok(info)
}

#[derive(Debug, thiserror::Error)]
pub enum OemError {
    #[error("OEM requires platform.soc=rk3588")]
    Unsupported,
    #[error("{0}")]
    Read(#[from] io::Error),
}

#[derive(Debug)]
pub struct Oem {
    serial: [u8; 16],
    mac: [u8; 6],
}

#[derive(Debug, Serialize)]
pub struct OemInfo {
    pub sn: String,
    pub mac: String,
}

impl Oem {
    pub fn read(soc: &str) -> Result<Self, OemError> {
        if soc != "rk3588" {
            return Err(OemError::Unsupported);
        }
        Ok(Self::decode(File::open(OEM_OTP)?)?)
    }

    fn decode(mut reader: impl Read) -> io::Result<Self> {
        let mut aligned = [0; 24];
        reader.read_exact(&mut aligned)?;
        let mut serial = [0; 16];
        let mut mac = [0; 6];
        serial.copy_from_slice(&aligned[..16]);
        mac.copy_from_slice(&aligned[16..22]);
        Ok(Self { serial, mac })
    }

    pub fn info(&self) -> OemInfo {
        let sn = self
            .serial
            .iter()
            .map(|&c| {
                if (0x20..=0x7e).contains(&c) {
                    char::from(c)
                } else {
                    '.'
                }
            })
            .collect();
        let mac = self.mac.map(|byte| format!("{byte:02X}")).join(":");
        OemInfo { sn, mac }
    }
}

fn valid_serial(serial: &[u8]) -> bool {
    serial.len() == 16
        && serial != b"0000000000000000"
        && serial
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(byte))
}

fn hostname_serial(serial: &[u8; 16]) -> Result<&str> {
    ensure!(serial != b"0000000000000000", "OTP SN is all zero");
    ensure!(
        serial.iter().enumerate().all(
            |(i, byte)| byte.is_ascii_alphanumeric() || (*byte == b'-' && (1..15).contains(&i))
        ),
        "OTP SN is not a valid hostname"
    );
    Ok(std::str::from_utf8(serial)?)
}

pub fn hostname() -> io::Result<String> {
    let mut buffer = [0u8; 256];
    // Linux writes at most the length supplied and NUL-terminates a fitting hostname.
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let end = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf8(buffer[..end].to_vec())
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))
}

pub fn apply_hostname(soc: &str) -> Result<String> {
    let oem = Oem::read(soc)?;
    let name = hostname_serial(&oem.serial)?;
    if hostname()? != name {
        // The validated ASCII name lives for the call. sethostname takes its exact length.
        if unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) } != 0 {
            return Err(io::Error::last_os_error()).context("set Linux hostname");
        }
        if let Err(error) = sync_avahi(name) {
            eprintln!("naos-core: Avahi hostname: {error}");
        }
    }
    if let Err(error) = persist_hostname(Path::new("/etc/hostname"), name) {
        eprintln!("naos-core: persist hostname: {error}");
    }
    Ok(name.to_owned())
}

fn persist_hostname(path: &Path, name: &str) -> io::Result<()> {
    match fs::read_to_string(path) {
        Ok(old) if old.trim() == name => return Ok(()),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    OpenOptions::new()
        .mode(0o644)
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?
        .write_all(format!("{name}\n").as_bytes())
}

fn sync_avahi(name: &str) -> Result<()> {
    if !Path::new("/usr/sbin/avahi-daemon").try_exists()? {
        return Ok(());
    }
    let mut command = Command::new("/usr/bin/dbus-send");
    command
        .args([
            "--system",
            "--print-reply",
            "--reply-timeout=1000",
            "--dest=org.freedesktop.Avahi",
            "/",
            "org.freedesktop.Avahi.Server.SetHostName",
            &format!("string:{name}"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    ensure!(
        wait(&mut command.spawn()?, Duration::from_secs(2))?.success(),
        "Avahi rejected hostname"
    );
    Ok(())
}

pub fn fresh_controller_id() -> Result<String> {
    let soc = [
        "/proc/device-tree/compatible",
        "/sys/firmware/devicetree/base/compatible",
    ]
    .iter()
    .find_map(|path| {
        let mut data = Vec::new();
        File::open(path)
            .ok()?
            .take(16385)
            .read_to_end(&mut data)
            .ok()?;
        (data.len() <= 16384
            && data
                .split(|&b| b == 0)
                .any(|item| item == b"rockchip,rk3588"))
        .then_some("rk3588")
    })
    .unwrap_or_default();
    controller_id(soc, "")
}

pub fn controller_id(soc: &str, known_serial: &str) -> Result<String> {
    if valid_serial(known_serial.as_bytes()) {
        return Ok(format!("SN--{known_serial}"));
    }
    if let Ok(oem) = Oem::read(soc)
        && valid_serial(&oem.serial)
    {
        return Ok(format!("SN--{}", std::str::from_utf8(&oem.serial)?));
    }
    random_controller_id(File::open("/dev/urandom")?)
}

fn random_controller_id(mut random: impl Read) -> Result<String> {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut id = String::from("NN");
    let mut bytes = [0; 32];
    while id.len() < 20 {
        random
            .read_exact(&mut bytes)
            .context("read controller ID entropy")?;
        for &byte in bytes.iter().filter(|&&b| b < 248).take(20 - id.len()) {
            id.push(ALPHABET[usize::from(byte) % ALPHABET.len()].into());
        }
    }
    Ok(id)
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("{0}")]
    BeforeWrite(#[from] anyhow::Error),
    #[error("recovery state requires inspection: {0}")]
    Uncertain(io::Error),
}

pub fn request_recovery() -> Result<(), RecoveryError> {
    let path = fs::canonicalize(MISC).context("resolve misc device")?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .context("open misc device")?;
    let info = file.metadata().context("inspect misc descriptor")?;
    let current = fs::metadata(MISC).context("inspect misc path")?;
    if !info.file_type().is_block_device()
        || (info.dev(), info.ino()) != (current.dev(), current.ino())
    {
        return Err(anyhow::anyhow!("misc device changed during validation").into());
    }
    let mut capacity: u64 = 0;
    // BLKGETSIZE64 writes a u64 to the supplied storage, and is valid on Linux ARM64 and x86-64.
    let status = unsafe { libc::ioctl(file.as_raw_fd(), 0x8008_1272_u32 as _, &mut capacity) };
    if status != 0 || capacity != 4 * 1024 * 1024 {
        return Err(anyhow::anyhow!("opened misc capacity does not match layout").into());
    }
    arm_recovery(&file)
}

fn recovery_record() -> [u8; RECOVERY_SIZE] {
    let mut record = [0; RECOVERY_SIZE];
    record[..256].fill(b' ');
    let header = b"NAOS-RECOVERY\nversion=3\noperation=clear-userdata";
    record[..header.len()].copy_from_slice(header);
    let hash = format!("{:x}", Sha256::digest(&record[..256]));
    record[256..320].copy_from_slice(hash.as_bytes());
    record
}

fn arm_recovery(file: &File) -> Result<(), RecoveryError> {
    let wanted = recovery_record();
    let mut contents = [0; RECOVERY_SIZE];
    file.read_exact_at(&mut contents, RECOVERY_OFFSET)
        .context("read recovery record")?;
    if contents != [0; RECOVERY_SIZE] && contents != wanted {
        return Err(anyhow::anyhow!("misc contains an unknown or damaged recovery record").into());
    }
    if contents != wanted {
        file.write_all_at(&wanted, RECOVERY_OFFSET)
            .map_err(RecoveryError::Uncertain)?;
    }
    file.sync_all().map_err(RecoveryError::Uncertain)?;
    file.read_exact_at(&mut contents, RECOVERY_OFFSET)
        .map_err(RecoveryError::Uncertain)?;
    if contents != wanted {
        return Err(RecoveryError::Uncertain(io::Error::other(
            "recovery record readback differs",
        )));
    }
    Ok(())
}

pub fn reboot() -> Result<()> {
    ensure!(
        Command::new("/sbin/reboot").status()?.success(),
        "reboot command failed"
    );
    Ok(())
}

pub fn boot_check(
    path: &Path,
    timeout: Duration,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    ensure!(
        !path.as_os_str().is_empty(),
        "check_program is required when check_enabled=true"
    );
    let info = fs::metadata(path)?;
    ensure!(
        info.is_file() && info.permissions().mode() & 0o111 != 0,
        "check_program must be an executable file (scripts need a shebang)"
    );
    let mut child = Command::new(path)
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let result = stream_boot_check(&mut child, timeout, out, err);
    if result.is_err() {
        // Always reap the child and release descendants holding output pipes.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.wait();
    }
    result
}

fn stream_boot_check(
    child: &mut Child,
    timeout: Duration,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<()> {
    let mut stdout = child.stdout.take().context("missing boot check stdout")?;
    let mut stderr = child.stderr.take().context("missing boot check stderr")?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let deadline = Instant::now() + timeout;
    let mut completed = None;
    let mut output_deadline = deadline;
    let (mut stdout_closed, mut stderr_closed) = (false, false);
    loop {
        if !stdout_closed {
            stdout_closed = drain_output(&mut stdout, out)?;
        }
        if !stderr_closed {
            stderr_closed = drain_output(&mut stderr, err)?;
        }
        if completed.is_none()
            && let Some(status) = child.try_wait()?
        {
            completed = Some(status);
            output_deadline = Instant::now() + Duration::from_secs(1);
        }
        if let Some(status) = completed {
            if stdout_closed && stderr_closed {
                ensure!(status.success(), "{status}");
                return Ok(());
            }
            ensure!(
                Instant::now() < output_deadline,
                "check output did not close after program exit"
            );
        } else {
            ensure!(
                Instant::now() < deadline,
                "check did not complete: deadline exceeded"
            );
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn nonblocking(file: &impl AsRawFd) -> io::Result<()> {
    // These descriptors belong exclusively to this process's child output.
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_output(reader: &mut impl Read, writer: &mut dyn Write) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    match reader.read(&mut buffer) {
        Ok(0) => Ok(true),
        Ok(length) => {
            writer.write_all(&buffer[..length])?;
            writer.flush()?;
            Ok(false)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn wait(child: &mut Child, timeout: Duration) -> Result<ExitStatus> {
    let end = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= end {
            // process_group(0) puts the child and its descendants in a separate group.
            unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
            let _ = child.wait();
            bail!("check did not complete: deadline exceeded");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_metadata_is_bounded_trimmed_and_requires_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("version.json");
        fs::write(
            &path,
            br#"{"version":"  1.2  ","build_id":null,"machine":"board","extra":2}"#,
        )
        .unwrap();
        assert_eq!(
            system_info(&path).unwrap(),
            SystemInfo {
                version: "1.2".into(),
                machine: "board".into(),
                ..Default::default()
            }
        );
        for bad in [
            br#"{"version":4}"#.to_vec(),
            br#"{"version":"  "}"#.to_vec(),
            vec![b' '; 65537],
        ] {
            fs::write(&path, bad).unwrap();
            assert!(system_info(&path).is_err());
        }
    }

    #[test]
    fn otp_layout_and_serial_rules_match_device_contract() {
        let mut data = [0; 24];
        data[..16].copy_from_slice(b"NAOS-00000000001");
        data[16..22].copy_from_slice(&[0, 1, 2, 0xaa, 0xbb, 0xff]);
        let otp = Oem::decode(&data[..]).unwrap();
        assert_eq!(otp.info().mac, "00:01:02:AA:BB:FF");
        assert_eq!(hostname_serial(&otp.serial).unwrap(), "NAOS-00000000001");
        assert!(Oem::decode(&data[..23]).is_err());
        assert!(matches!(Oem::read("qcs9075"), Err(OemError::Unsupported)));
        assert!(valid_serial(b"NAOS_00000000001"));
        assert!(hostname_serial(b"NAOS_00000000001").is_err());
        assert!(!valid_serial(b"0000000000000000"));
    }

    #[test]
    fn random_id_rejects_biased_bytes() {
        let mut entropy = vec![255; 32];
        entropy.extend([1; 32]);
        assert_eq!(
            random_controller_id(&entropy[..]).unwrap(),
            "NN111111111111111111"
        );
        assert!(random_controller_id(&[][..]).is_err());
    }

    #[test]
    fn recovery_is_idempotent_and_refuses_to_replace_unknown_sector() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(RECOVERY_OFFSET + 1024).unwrap();
        file.write_all_at(b"x", RECOVERY_OFFSET - 1).unwrap();
        arm_recovery(&file).unwrap();
        arm_recovery(&file).unwrap();
        let mut data = [0; RECOVERY_SIZE];
        file.read_exact_at(&mut data, RECOVERY_OFFSET).unwrap();
        assert_eq!(data, recovery_record());
        assert_eq!(
            &data[256..320],
            format!("{:x}", Sha256::digest(&data[..256])).as_bytes()
        );
        file.write_all_at(b"y", RECOVERY_OFFSET).unwrap();
        assert!(matches!(
            arm_recovery(&file),
            Err(RecoveryError::BeforeWrite(_))
        ));
        let mut boundary = [0; 2];
        file.read_exact_at(&mut boundary, RECOVERY_OFFSET - 1)
            .unwrap();
        assert_eq!(&boundary, b"xy");
    }

    #[test]
    fn executable_boot_check_reports_error_and_times_out_as_a_process_group() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("boot-check");
        let mut out = Vec::new();
        let mut err = Vec::new();
        fs::write(
            &path,
            "#!/bin/sh\nprintf ready\nprintf warning >&2\nexit 4\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(boot_check(&path, Duration::from_secs(2), &mut out, &mut err).is_err());
        assert_eq!(out, b"ready");
        assert_eq!(err, b"warning");
        fs::write(&path, "#!/bin/sh\nsleep 10 &\nwait\n").unwrap();
        let started = Instant::now();
        assert!(
            boot_check(&path, Duration::from_millis(40), &mut out, &mut err)
                .unwrap_err()
                .to_string()
                .contains("deadline exceeded")
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn boot_check_streams_output_before_the_program_can_finish() {
        struct SignalOutput(std::path::PathBuf);
        impl Write for SignalOutput {
            fn write(&mut self, data: &[u8]) -> io::Result<usize> {
                fs::write(&self.0, data)?;
                Ok(data.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("boot-check");
        let signal = root.path().join("observed");
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf ready\nwhile [ ! -e '{}' ]; do sleep 0.01; done\n",
                signal.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        boot_check(
            &path,
            Duration::from_secs(2),
            &mut SignalOutput(signal.clone()),
            &mut io::sink(),
        )
        .unwrap();
        assert_eq!(fs::read(signal).unwrap(), b"ready");
    }

    #[test]
    fn boot_check_stops_waiting_for_descendants_holding_pipes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("boot-check");
        fs::write(&path, "#!/bin/sh\nsleep 60 &\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let start = Instant::now();
        let error = boot_check(
            &path,
            Duration::from_secs(4),
            &mut io::sink(),
            &mut io::sink(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("output did not close"));
        assert!(start.elapsed() < Duration::from_secs(3));
    }
}
