use super::*;
use std::io::Cursor;
use std::os::unix::net::UnixListener;

#[derive(Default)]
struct MockStream {
    incoming: Cursor<Vec<u8>>,
    outgoing: Vec<u8>,
}

impl Read for MockStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.incoming.read(buffer)
    }
}

impl Write for MockStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // Force short writes on both the length prefix and the body.
        let count = bytes.len().min(2);
        self.outgoing.extend_from_slice(&bytes[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn frame(kind: u8, method: &str, code: u8, message: &str, payload: &[u8]) -> Vec<u8> {
    let mut fields = vec![8, kind, 32, code, 48, 1];
    if !method.is_empty() {
        push_bytes(&mut fields, 2, method.as_bytes());
    }
    if !message.is_empty() {
        push_bytes(&mut fields, 5, message.as_bytes());
    }
    if !payload.is_empty() {
        push_bytes(&mut fields, 3, payload);
    }
    fields
}

fn mock(frames: &[Vec<u8>]) -> MockStream {
    let mut buffer = Vec::new();
    for frame in frames {
        write_frame(&mut buffer, frame).unwrap();
    }
    MockStream {
        incoming: Cursor::new(buffer),
        outgoing: Vec::new(),
    }
}

#[test]
fn list_requires_matching_result_and_serializes_empty_array() {
    let mut stream = mock(&[frame(4, "List", 0, "", &[])]);
    assert_eq!(
        serde_json::to_string(&list(&mut stream).unwrap()).unwrap(),
        "[]"
    );
    assert_eq!(stream.outgoing, b"\0\0\0\x0a\x08\x01\x12\x04List\x30\x01");

    let payload = b"\x0a\x04demo\x10\xaa\x02\x18\x01\x22\x07running\x42\x04root\x50\x20";
    let data = frame(2, "", 0, "", payload);
    let mut incomplete = mock(std::slice::from_ref(&data));
    assert!(list(&mut incomplete).is_err());
    let mut valid = mock(&[data, frame(4, "List", 0, "", &[])]);
    let states = list(&mut valid).unwrap();
    assert_eq!(
        states,
        vec![State {
            id: "demo".into(),
            pid: 298,
            running: true,
            status: "running".into(),
            owner: "root".into(),
            process_start_time: 32,
            ..State::default()
        }]
    );
}

#[test]
fn list_bounds_data_and_response_frames() {
    let data = frame(2, "List", 0, "", b"\x0a\x04demo");
    let mut limit = vec![data.clone(); MAX_STATES];
    limit.push(frame(4, "", 0, "", &[]));
    assert_eq!(list(&mut mock(&limit)).unwrap().len(), MAX_STATES);
    limit.insert(0, data);
    assert!(list(&mut mock(&limit)).is_err());
    assert!(
        list(&mut mock(&[frame(
            2,
            "List",
            0,
            "",
            &vec![0; MAX_LIST_BYTES + 1]
        )]))
        .is_err()
    );
}

#[test]
fn control_uses_expected_protobuf_value_and_does_not_retry_failure() {
    for method in ["Start", "Stop", "Restart"] {
        let mut stream = mock(&[frame(4, method, 0, "", &[])]);
        control(&mut stream, method, "demo").unwrap();
        let frame = read_frame(&mut stream.outgoing.as_slice()).unwrap();
        assert_eq!(frame.kind, 1);
        assert_eq!(frame.method, method);
        assert_eq!(frame.payload, b"\x0a\x04demo");
    }
    let message = "x".repeat(275);
    let mut remote = mock(&[frame(4, "Start", 5, &message, &[])]);
    let failure = control(&mut remote, "Start", "demo").unwrap_err();
    assert!(matches!(failure, Error::Remote { code: 5, ref message } if message.len() == 256));
    let mut written = remote.outgoing.as_slice();
    assert!(read_frame(&mut written).is_ok());
    assert!(written.is_empty());
    assert!(control(&mut mock(&[frame(4, "Stop", 0, "", &[])]), "Start", "demo").is_err());
    assert!(control(&mut mock(&[frame(2, "Start", 0, "", &[])]), "Start", "demo").is_err());
}

#[test]
fn rejects_malformed_and_excessive_protobuf_without_parsing_untrusted_text() {
    for payload in [
        b"\0".as_slice(),
        b"\x0a\x12truncated",
        b"\x0a\x01\xff",
        b"\x10\x80\x80\x80\x80\x10",
        b"\x10\xff\xff\xff\xff\xff\xff\xff\xff\xff\x02",
        b"\x14",
    ] {
        assert!(decode_state(payload).is_err(), "{payload:?}");
    }
    for payload in [
        b"\x08\x05\x30\x01".as_slice(),
        b"\x08\x04\x30\x02",
        b"\x08\x04\x20\x11\x30\x01",
    ] {
        assert!(decode_frame(payload).is_err());
    }
    let prefix = ((MAX_FRAME + 1) as u32).to_be_bytes();
    assert!(read_frame(&mut prefix.as_slice()).is_err());
}

#[test]
fn accepts_unknown_fields_supported_wire_types() {
    let mut payload = b"\x0a\x04demo".to_vec();
    push_varint(&mut payload, (128 << 3) + 1);
    payload.extend([1; 8]);
    push_varint(&mut payload, (129 << 3) + 5);
    payload.extend([1; 4]);
    assert_eq!(decode_state(&payload).unwrap().id, "demo");
}

#[test]
fn rejects_unsafe_ids_and_manifest_paths_before_connect() {
    let client = Client {
        socket: "/nonexistent/socket".into(),
    };
    for id in ["", "..", "-demo", "/demo", "demo/test", "日本", "a b"] {
        assert!(
            matches!(client.change(id, "start"), Err(Error::InvalidId)),
            "{id}"
        );
    }
    assert!(matches!(
        client.change("demo", "remove"),
        Err(Error::InvalidAction)
    ));
    for path in [
        "/tmp/manifest.yml",
        "/userdata/../tmp/manifest.yml",
        "/userdata//manifest.yml",
        "/userdata/./manifest.yml",
        "/userdata/test\n/manifest.yml",
        "userdata/manifest.yml",
        "/var/lib/naos/containers/image/extra/manifest.yml",
    ] {
        assert!(
            matches!(client.start_manifest(path), Err(Error::InvalidManifest)),
            "{path}"
        );
    }
    for path in [
        "/userdata/manifest.yml",
        "/userdata/foo/manifest.yml",
        "/var/lib/naos/containers/a_B-c.1/manifest.yml",
    ] {
        assert!(valid_manifest_path(path), "{path}");
    }
}

#[test]
fn unix_socket_handles_one_manifest_start_and_list_without_touching_naosd() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fake-naosd.sock");
    let listener = UnixListener::bind(&path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let accept = || {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((socket, _)) => return socket,
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("mock naosd did not accept a connection: {error}"),
            }
        }
    };
    std::thread::scope(|scope| {
        let daemon = scope.spawn(|| {
            let mut start = accept();
            start
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let request = read_frame(&mut start).unwrap();
            assert_eq!(request.method, "Start");
            let mut manifest = None;
            fields(&request.payload, |number, field| {
                if number == 1 {
                    manifest = Some(field.text()?);
                }
                Ok(())
            })
            .unwrap();
            assert_eq!(manifest.as_deref(), Some("/userdata/manifest.yml"));
            write_frame(&mut start, &frame(4, "Start", 0, "", &[])).unwrap();

            let mut list = accept();
            list.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
            assert_eq!(read_frame(&mut list).unwrap().method, "List");
            write_frame(&mut list, &frame(4, "List", 0, "", &[])).unwrap();
        });
        let client = Client {
            socket: path.to_str().unwrap().into(),
        };
        client.start_manifest("/userdata/manifest.yml").unwrap();
        assert!(client.list().unwrap().is_empty());
        daemon.join().unwrap();
    });
}

#[test]
fn absolute_deadline_is_enforced_even_with_connection_open() {
    let (stream, _peer) = UnixStream::pair().unwrap();
    let mut connection = Connection {
        stream,
        deadline: Instant::now(),
    };
    assert_eq!(
        connection.read(&mut [0; 1]).unwrap_err().kind(),
        io::ErrorKind::TimedOut
    );
}

#[test]
fn stop_and_restart_wait_past_the_old_budget_for_a_completion_result() {
    // Scale down seconds for the socket fixture while retaining the
    // relationship between the former budget and the graceful window.
    let scaled = |duration: Duration| Duration::from_millis(duration.as_secs() * 20);
    let old_budget = scaled(Duration::from_secs(10));
    for action in ["stop", "restart"] {
        let (method, deadline) = control_action(action).unwrap();
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let fake_daemon = std::thread::spawn(move || {
            peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            assert_eq!(read_frame(&mut peer).unwrap().method, method);
            std::thread::sleep(old_budget + Duration::from_millis(25));
            write_frame(&mut peer, &frame(4, method, 0, "", &[])).unwrap();
        });
        let began = Instant::now();
        let mut connection = Connection {
            stream,
            deadline: began + scaled(deadline),
        };
        control(&mut connection, method, "demo").unwrap();
        assert!(began.elapsed() > old_budget);
        fake_daemon.join().unwrap();
    }
}

#[test]
fn a_sent_operation_with_no_result_has_unconfirmed_outcome() {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let fake_daemon = std::thread::spawn(move || {
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(read_frame(&mut peer).unwrap().method, "Restart");
        // The daemon can change container state before the socket is lost.
    });
    let mut connection = Connection {
        stream,
        deadline: Instant::now() + Duration::from_secs(2),
    };
    assert!(matches!(
        control(&mut connection, "Restart", "demo"),
        Err(Error::OutcomeUnconfirmed(error)) if error.kind() == io::ErrorKind::UnexpectedEof
    ));
    fake_daemon.join().unwrap();
}
