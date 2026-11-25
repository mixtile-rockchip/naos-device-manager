use serde::Serialize;
use std::io::{self, Read, Write};
use std::mem::{offset_of, size_of};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const MAX_FRAME: usize = 1024 * 1024;
const MAX_LIST_BYTES: usize = 256 * 1024;
const MAX_STATES: usize = 64;
const MAX_RESPONSE_FRAMES: usize = 66;
const LIST_TIMEOUT: Duration = Duration::from_secs(3);
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// naosd's default 10-second graceful stop precedes forced cleanup and restart.
pub const GRACEFUL_CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct State {
    pub id: String,
    pub pid: u64,
    pub running: bool,
    pub status: String,
    pub bundle: String,
    pub rootfs: String,
    pub created: String,
    pub owner: String,
    pub cgroup_path: String,
    pub process_start_time: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "container id must be 1..128 ASCII letters, digits, dot, underscore or hyphen, starting with a letter or digit"
    )]
    InvalidId,
    #[error(
        "container manifest must be a clean /userdata/ path or /var/lib/naos/containers/<id>/manifest.yml"
    )]
    InvalidManifest,
    #[error("unsupported container operation")]
    InvalidAction,
    #[error("naosd operation failed (code {code}): {message}")]
    Remote { code: u64, message: String },
    #[error("connect naosd: {0}")]
    Connect(#[source] io::Error),
    #[error("naosd protocol: {0}")]
    Protocol(#[from] io::Error),
    #[error(
        "naosd did not confirm the outcome ({0}). The operation may have changed container state."
    )]
    OutcomeUnconfirmed(#[source] io::Error),
}

#[derive(Clone, Debug)]
pub struct Client {
    pub socket: String,
}

impl Client {
    pub fn list(&self) -> Result<Vec<State>, Error> {
        list(&mut Connection::open(&self.socket, LIST_TIMEOUT)?)
    }

    pub fn change(&self, id: &str, action: &str) -> Result<(), Error> {
        if !valid_id(id) {
            return Err(Error::InvalidId);
        }
        let (method, timeout) = control_action(action)?;
        control(&mut Connection::open(&self.socket, timeout)?, method, id)
    }

    pub fn start_manifest(&self, path: &str) -> Result<(), Error> {
        if !valid_manifest_path(path) {
            return Err(Error::InvalidManifest);
        }
        control(
            &mut Connection::open(&self.socket, START_TIMEOUT)?,
            "Start",
            path,
        )
    }
}

fn control_action(action: &str) -> Result<(&'static str, Duration), Error> {
    match action {
        "start" => Ok(("Start", START_TIMEOUT)),
        "stop" => Ok(("Stop", GRACEFUL_CONTROL_TIMEOUT)),
        "restart" => Ok(("Restart", GRACEFUL_CONTROL_TIMEOUT)),
        _ => Err(Error::InvalidAction),
    }
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id.as_bytes()[0].is_ascii_alphanumeric()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn valid_manifest_path(path: &str) -> bool {
    if path.len() > 4096
        || path.chars().any(char::is_control)
        || !path.ends_with("/manifest.yml")
        || !path.starts_with('/')
        || !path[1..]
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
    {
        return false;
    }
    if path.starts_with("/userdata/") {
        return true;
    }
    path.strip_prefix("/var/lib/naos/containers/")
        .and_then(|relative| relative.strip_suffix("/manifest.yml"))
        .is_some_and(valid_id)
}

/// A single deadline covers every frame and each partial read or write.
struct Connection {
    stream: UnixStream,
    deadline: Instant,
}

impl Connection {
    fn open(path: &str, timeout: Duration) -> Result<Self, Error> {
        let deadline = Instant::now() + timeout;
        // Use a nonblocking connect so a full local listen queue cannot stall
        // the operation. Socket I/O becomes blocking once connected.
        let stream = connect(path).map_err(Error::Connect)?;
        stream.set_nonblocking(false)?;
        Ok(Self { stream, deadline })
    }

    fn remaining(&self) -> io::Result<Duration> {
        let duration = self.deadline.saturating_duration_since(Instant::now());
        if duration.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        Ok(duration)
    }
}

impl Read for Connection {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(buffer)
    }
}

impl Write for Connection {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

fn connect(path: &str) -> io::Result<UnixStream> {
    // SAFETY: a zeroed sockaddr_un represents an empty pathname address.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if path.is_empty() || path.len() >= address.sun_path.len() || path.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid naosd socket path",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (target, byte) in address.sun_path.iter_mut().zip(path.bytes()) {
        *target = byte as libc::c_char;
    }
    let length = offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1;
    debug_assert!(length <= size_of::<libc::sockaddr_un>());
    // SAFETY: socket takes scalar arguments, and each successful descriptor is
    // immediately wrapped in an OwnedFd. connect receives a live pathname address.
    let raw = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let socket = unsafe { OwnedFd::from_raw_fd(raw) };
    let status = unsafe {
        libc::connect(
            socket.as_raw_fd(),
            &raw const address as *const libc::sockaddr,
            length as libc::socklen_t,
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(UnixStream::from(socket))
}

fn list(conn: &mut (impl Read + Write)) -> Result<Vec<State>, Error> {
    write_frame(conn, b"\x08\x01\x12\x04List\x30\x01")?;
    let mut states = Vec::new();
    let mut total = 0;
    for _ in 0..MAX_RESPONSE_FRAMES {
        let frame = read_frame(conn)?;
        if !frame.method.is_empty() && frame.method != "List" {
            return Err(invalid("response method differs from List").into());
        }
        match frame.kind {
            2 => {
                total += frame.payload.len();
                if states.len() >= MAX_STATES || total > MAX_LIST_BYTES {
                    return Err(invalid("List exceeds 64 states or 256 KiB").into());
                }
                states.push(decode_state(&frame.payload)?);
            }
            4 if frame.code == 0 => return Ok(states),
            4 => return Err(remote_error(frame)),
            _ => return Err(invalid("List expected DATA or RESULT").into()),
        }
    }
    Err(invalid("List response frame count exceeded").into())
}

fn control(conn: &mut (impl Read + Write), method: &str, value: &str) -> Result<(), Error> {
    let mut payload = Vec::new();
    push_bytes(&mut payload, 1, value.as_bytes());
    let mut request = vec![8, 1];
    push_bytes(&mut request, 2, method.as_bytes());
    push_bytes(&mut request, 3, &payload);
    request.extend_from_slice(&[48, 1]);
    write_frame(conn, &request)?;
    let frame = read_frame(conn).map_err(|error| match error.kind() {
        io::ErrorKind::TimedOut
        | io::ErrorKind::WouldBlock
        | io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset => Error::OutcomeUnconfirmed(error),
        _ => Error::Protocol(error),
    })?;
    if frame.kind != 4 || (!frame.method.is_empty() && frame.method != method) {
        return Err(invalid("control response is not the matching RESULT").into());
    }
    if frame.code != 0 {
        return Err(remote_error(frame));
    }
    Ok(())
}

fn remote_error(frame: Frame) -> Error {
    Error::Remote {
        code: frame.code,
        message: frame.message.chars().take(256).collect(),
    }
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn push_varint(output: &mut Vec<u8>, mut value: u64) {
    while value >= 128 {
        output.push((value as u8) | 128);
        value >>= 7;
    }
    output.push(value as u8);
}

fn push_bytes(output: &mut Vec<u8>, number: u8, bytes: &[u8]) {
    output.push((number << 3) | 2);
    push_varint(output, bytes.len() as u64);
    output.extend_from_slice(bytes);
}

fn take_varint(data: &mut &[u8]) -> io::Result<u64> {
    let mut value = 0;
    for shift in (0..=63).step_by(7) {
        let (&byte, suffix) = data
            .split_first()
            .ok_or(invalid("truncated protobuf varint"))?;
        *data = suffix;
        if shift == 63 && byte > 1 {
            return Err(invalid("protobuf varint overflow"));
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err(invalid("protobuf varint overflow"))
}

enum Field<'a> {
    Integer(u64),
    Bytes(&'a [u8]),
    Fixed,
}

impl<'a> Field<'a> {
    fn integer(&self) -> io::Result<u64> {
        match self {
            Self::Integer(value) => Ok(*value),
            _ => Err(invalid("invalid protobuf integer field")),
        }
    }

    fn bytes(&self) -> io::Result<&'a [u8]> {
        match self {
            Self::Bytes(value) => Ok(value),
            _ => Err(invalid("invalid protobuf bytes field")),
        }
    }

    fn text(&self) -> io::Result<String> {
        std::str::from_utf8(self.bytes()?)
            .map(str::to_owned)
            .map_err(|_| invalid("invalid protobuf UTF-8 string field"))
    }
}

fn take_bytes<'a>(data: &mut &'a [u8], length: usize) -> io::Result<&'a [u8]> {
    let (bytes, remaining) = data
        .split_at_checked(length)
        .ok_or(io::ErrorKind::UnexpectedEof)?;
    *data = remaining;
    Ok(bytes)
}

fn fields(
    mut data: &[u8],
    mut visit: impl FnMut(u64, Field<'_>) -> io::Result<()>,
) -> io::Result<()> {
    while !data.is_empty() {
        let key = take_varint(&mut data)?;
        let number = key >> 3;
        if number == 0 || number >= (1 << 29) {
            return Err(invalid("invalid protobuf field key"));
        }
        let field = match key & 7 {
            0 => Field::Integer(take_varint(&mut data)?),
            2 => {
                let length = usize::try_from(take_varint(&mut data)?)
                    .map_err(|_| invalid("invalid protobuf byte length"))?;
                Field::Bytes(take_bytes(&mut data, length)?)
            }
            1 | 5 => {
                take_bytes(&mut data, if key & 7 == 1 { 8 } else { 4 })?;
                Field::Fixed
            }
            _ => return Err(invalid("unsupported protobuf wire type")),
        };
        visit(number, field)?;
    }
    Ok(())
}

#[derive(Debug, Default)]
struct Frame {
    kind: u64,
    code: u64,
    version: u64,
    method: String,
    message: String,
    payload: Vec<u8>,
}

fn decode_frame(data: &[u8]) -> io::Result<Frame> {
    let mut frame = Frame::default();
    fields(data, |number, field| {
        match number {
            1 => frame.kind = field.integer()?,
            2 => frame.method = field.text()?,
            3 => frame.payload = field.bytes()?.to_vec(),
            4 => frame.code = field.integer()?,
            5 => frame.message = field.text()?,
            6 => frame.version = field.integer()?,
            _ => {}
        }
        Ok(())
    })?;
    if frame.version != 1 || !(1..=4).contains(&frame.kind) || frame.code > 16 {
        return Err(invalid("invalid frame version, kind or result code"));
    }
    Ok(frame)
}

fn decode_state(data: &[u8]) -> io::Result<State> {
    let mut state = State::default();
    fields(data, |number, field| {
        match number {
            1 => state.id = field.text()?,
            2 => {
                state.pid = field.integer()?;
                if state.pid > u64::from(u32::MAX) {
                    return Err(invalid("pid exceeds uint32"));
                }
            }
            3 => state.running = field.integer()? != 0,
            4 => state.status = field.text()?,
            5 => state.bundle = field.text()?,
            6 => state.rootfs = field.text()?,
            7 => state.created = field.text()?,
            8 => state.owner = field.text()?,
            9 => state.cgroup_path = field.text()?,
            10 => state.process_start_time = field.integer()?,
            _ => {}
        }
        Ok(())
    })?;
    Ok(state)
}

fn write_frame(writer: &mut impl Write, body: &[u8]) -> io::Result<()> {
    writer.write_all(&(body.len() as u32).to_be_bytes())?;
    writer.write_all(body)
}

fn read_frame(reader: &mut impl Read) -> io::Result<Frame> {
    let mut prefix = [0; 4];
    reader.read_exact(&mut prefix)?;
    let length = u32::from_be_bytes(prefix) as usize;
    if !(1..=MAX_FRAME).contains(&length) {
        return Err(invalid("frame length outside 1..1 MiB"));
    }
    let mut data = vec![0; length];
    reader.read_exact(&mut data)?;
    decode_frame(&data)
}

#[cfg(test)]
mod tests;
