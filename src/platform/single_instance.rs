use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

const SOCKET_FILE_NAME: &str = "instance.sock";
const XDG_ACTIVATION_TOKEN_ENV: &str = "XDG_ACTIVATION_TOKEN";
const PROTOCOL_VERSION: u8 = 1;
const EXTERNAL_LAUNCH_MESSAGE: u8 = 1;
const REQUEST_HEADER_LEN: usize = 4;
const MAX_ACTIVATION_TOKEN_BYTES: usize = 4096;
const CLAIM_RETRIES: usize = 8;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct ExternalLaunchRequest {
    pub(crate) activation_token: Option<String>,
}

impl std::fmt::Debug for ExternalLaunchRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExternalLaunchRequest")
            .field(
                "activation_token",
                &self.activation_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug)]
pub(crate) enum SingleInstanceStatus {
    Primary(PrimaryInstance),
    Secondary,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

struct StartupLock {
    _file: File,
}

impl StartupLock {
    fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        loop {
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if result == 0 {
                return Ok(Self { _file: file });
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct PrimaryInstance {
    socket_path: PathBuf,
    socket_identity: SocketIdentity,
    listener: Option<UnixListener>,
    listener_guard: Option<UnixListener>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl PrimaryInstance {
    pub(crate) fn start_listener<F>(&mut self, mut external_launch: F) -> io::Result<()>
    where
        F: FnMut(ExternalLaunchRequest) -> bool + Send + 'static,
    {
        if self.worker.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "single-instance listener is already running",
            ));
        }
        let listener = self.listener.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "single-instance listener is unavailable",
            )
        })?;
        let stop = Arc::clone(&self.stop);
        let worker = match super::spawn_named("ronsole-single-instance", move || {
            loop {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                };
                if stop.load(Ordering::Acquire) {
                    break;
                }

                let _ = stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT));
                if let Ok(request) = read_external_launch_request(&mut stream)
                    && !external_launch(request)
                {
                    break;
                }
            }
        }) {
            Ok(worker) => worker,
            Err(error) => {
                cleanup_socket_if_owned(&self.socket_path, self.socket_identity);
                return Err(error);
            }
        };
        self.worker = Some(worker);
        Ok(())
    }
}

impl Drop for PrimaryInstance {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if self.worker.is_some() {
            let _ = UnixStream::connect(&self.socket_path);
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        self.listener.take();
        cleanup_socket_if_owned(&self.socket_path, self.socket_identity);
        self.listener_guard.take();
    }
}

pub(crate) fn acquire_single_instance() -> io::Result<SingleInstanceStatus> {
    let uid = effective_uid();
    if let Some(preferred) = preferred_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR"))
        && prepare_private_runtime_dir(&preferred, uid).is_ok()
    {
        return claim_at(preferred.join(SOCKET_FILE_NAME), uid);
    }

    let fallback = fallback_runtime_dir(uid);
    prepare_private_runtime_dir(&fallback, uid)?;
    claim_at(fallback.join(SOCKET_FILE_NAME), uid)
}

fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

fn preferred_runtime_dir(value: Option<OsString>) -> Option<PathBuf> {
    value
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .map(|path| path.join("ronsole"))
}

fn fallback_runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/ronsole-runtime-{uid}"))
}

fn prepare_private_runtime_dir(path: &Path, uid: u32) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }

    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("runtime path is not a directory: {}", path.display()),
        ));
    }
    if metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "runtime directory is not owned by the current user: {}",
                path.display()
            ),
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn claim_at(socket_path: PathBuf, uid: u32) -> io::Result<SingleInstanceStatus> {
    claim_at_with_request(socket_path, uid, || ExternalLaunchRequest {
        activation_token: take_activation_token(),
    })
}

fn claim_at_with_request(
    socket_path: PathBuf,
    uid: u32,
    mut request: impl FnMut() -> ExternalLaunchRequest,
) -> io::Result<SingleInstanceStatus> {
    let _startup_lock = StartupLock::acquire(&startup_lock_path(&socket_path))?;
    for _ in 0..CLAIM_RETRIES {
        match notify_existing(&socket_path, &mut request) {
            Ok(()) => return Ok(SingleInstanceStatus::Secondary),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {
                remove_stale_socket(&socket_path, uid)?;
            }
            Err(error) => return Err(error),
        }

        match UnixListener::bind(&socket_path) {
            Ok(listener) => {
                let listener_guard = listener.try_clone()?;
                if let Err(error) =
                    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
                {
                    drop(listener);
                    let _ = fs::remove_file(&socket_path);
                    return Err(error);
                }
                let metadata = fs::symlink_metadata(&socket_path)?;
                let socket_identity = SocketIdentity {
                    device: metadata.dev(),
                    inode: metadata.ino(),
                };
                return Ok(SingleInstanceStatus::Primary(PrimaryInstance {
                    socket_path,
                    socket_identity,
                    listener: Some(listener),
                    listener_guard: Some(listener_guard),
                    stop: Arc::new(AtomicBool::new(false)),
                    worker: None,
                }));
            }
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => continue,
            Err(error) => return Err(error),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "single-instance ownership changed repeatedly during startup",
    ))
}

fn startup_lock_path(socket_path: &Path) -> PathBuf {
    socket_path.with_file_name("instance.lock")
}

fn notify_existing(
    socket_path: &Path,
    request: &mut impl FnMut() -> ExternalLaunchRequest,
) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
    let request = request();
    write_external_launch_request(&mut stream, &request)
}

fn take_activation_token() -> Option<String> {
    take_activation_token_with(
        || std::env::var_os(XDG_ACTIVATION_TOKEN_ENV),
        // SAFETY: production single-instance acquisition runs at process startup,
        // before Ronsole creates the event loop or starts worker threads.
        || unsafe { std::env::remove_var(XDG_ACTIVATION_TOKEN_ENV) },
    )
}

fn take_activation_token_with(
    mut get: impl FnMut() -> Option<OsString>,
    mut clear: impl FnMut(),
) -> Option<String> {
    let token = get();
    if token.is_some() {
        clear();
    }
    token
        .and_then(|token| token.into_string().ok())
        .filter(|token| !token.is_empty())
}

fn write_external_launch_request(
    writer: &mut impl Write,
    request: &ExternalLaunchRequest,
) -> io::Result<()> {
    let token = request
        .activation_token
        .as_deref()
        .filter(|token| !token.is_empty())
        .map(str::as_bytes)
        .unwrap_or_default();
    if token.len() > MAX_ACTIVATION_TOKEN_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "single-instance activation token exceeds protocol limit",
        ));
    }
    let payload_len = u16::try_from(token.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "single-instance activation token exceeds protocol framing limit",
        )
    })?;
    let payload_len = payload_len.to_be_bytes();
    let header = [
        PROTOCOL_VERSION,
        EXTERNAL_LAUNCH_MESSAGE,
        payload_len[0],
        payload_len[1],
    ];
    writer.write_all(&header)?;
    writer.write_all(token)
}

fn read_external_launch_request(reader: &mut impl Read) -> io::Result<ExternalLaunchRequest> {
    let mut header = [0_u8; REQUEST_HEADER_LEN];
    reader.read_exact(&mut header)?;
    if header[0] != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported single-instance protocol version",
        ));
    }
    if header[1] != EXTERNAL_LAUNCH_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported single-instance message type",
        ));
    }

    let payload_len = usize::from(u16::from_be_bytes([header[2], header[3]]));
    if payload_len > MAX_ACTIVATION_TOKEN_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "single-instance request exceeds protocol limit",
        ));
    }

    let mut payload = [0_u8; MAX_ACTIVATION_TOKEN_BYTES];
    reader.read_exact(&mut payload[..payload_len])?;
    let activation_token = if payload_len == 0 {
        None
    } else {
        Some(
            std::str::from_utf8(&payload[..payload_len])
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "single-instance activation token is not valid UTF-8",
                    )
                })?
                .to_owned(),
        )
    };
    Ok(ExternalLaunchRequest { activation_token })
}

fn remove_stale_socket(socket_path: &Path, uid: u32) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(socket_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_socket() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "single-instance path is not a socket: {}",
                socket_path.display()
            ),
        ));
    }
    if metadata.uid() != uid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "single-instance socket is not owned by the current user: {}",
                socket_path.display()
            ),
        ));
    }
    fs::remove_file(socket_path)
}

fn cleanup_socket_if_owned(socket_path: &Path, identity: SocketIdentity) {
    let Ok(metadata) = fs::symlink_metadata(socket_path) else {
        return;
    };
    if metadata.file_type().is_socket()
        && metadata.dev() == identity.device
        && metadata.ino() == identity.inode
    {
        let _ = fs::remove_file(socket_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::sync::mpsc;

    static TEST_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let sequence = TEST_DIR_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ronsole-single-instance-test-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path).expect("create single-instance test directory");
            Self(path)
        }

        fn socket_path(&self) -> PathBuf {
            self.0.join(SOCKET_FILE_NAME)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn take_primary(status: SingleInstanceStatus) -> PrimaryInstance {
        match status {
            SingleInstanceStatus::Primary(primary) => primary,
            SingleInstanceStatus::Secondary => panic!("expected primary instance"),
        }
    }

    fn claim_without_activation_token(
        socket_path: PathBuf,
        uid: u32,
    ) -> io::Result<SingleInstanceStatus> {
        claim_at_with_request(socket_path, uid, ExternalLaunchRequest::default)
    }

    fn round_trip_request(request: &ExternalLaunchRequest) -> io::Result<ExternalLaunchRequest> {
        let mut encoded = Vec::new();
        write_external_launch_request(&mut encoded, request)?;
        read_external_launch_request(&mut Cursor::new(encoded))
    }

    fn send_request(path: &Path, request: &ExternalLaunchRequest) {
        let mut stream = UnixStream::connect(path).unwrap();
        stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT)).unwrap();
        write_external_launch_request(&mut stream, request).unwrap();
    }

    #[test]
    fn runtime_directory_prefers_nonempty_xdg_and_has_per_user_fallback() {
        assert_eq!(
            preferred_runtime_dir(Some(OsString::from("/run/user/1000"))),
            Some(PathBuf::from("/run/user/1000/ronsole"))
        );
        assert_eq!(preferred_runtime_dir(Some(OsString::new())), None);
        assert_eq!(
            fallback_runtime_dir(42),
            PathBuf::from("/tmp/ronsole-runtime-42")
        );
    }

    #[test]
    fn prepared_runtime_directory_is_private_to_current_user() {
        let dir = TestDir::new();
        let runtime = dir.0.join("runtime");
        prepare_private_runtime_dir(&runtime, effective_uid()).unwrap();
        let metadata = fs::symlink_metadata(runtime).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn external_launch_request_round_trips_without_token() {
        let request = ExternalLaunchRequest::default();
        let decoded = round_trip_request(&request).unwrap();
        assert!(decoded.activation_token.is_none());
    }

    #[test]
    fn external_launch_request_round_trips_with_token() {
        let request = ExternalLaunchRequest {
            activation_token: Some("wayland-token-123".to_owned()),
        };
        let decoded = round_trip_request(&request).unwrap();
        assert_eq!(
            decoded.activation_token.as_deref(),
            Some("wayland-token-123")
        );
    }

    #[test]
    fn external_launch_request_debug_redacts_activation_token() {
        let request = ExternalLaunchRequest {
            activation_token: Some("secret-activation-token".to_owned()),
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-activation-token"));
    }

    #[test]
    fn activation_token_capture_clears_environment_and_normalizes_empty_value() {
        let value = RefCell::new(Some(OsString::from("wayland-token-123")));
        let clear_calls = Cell::new(0_u8);
        let token = take_activation_token_with(
            || value.borrow().clone(),
            || {
                clear_calls.set(clear_calls.get() + 1);
                *value.borrow_mut() = None;
            },
        );
        assert_eq!(token.as_deref(), Some("wayland-token-123"));
        assert!(value.borrow().is_none());
        assert_eq!(clear_calls.get(), 1);

        let value = RefCell::new(Some(OsString::new()));
        let token =
            take_activation_token_with(|| value.borrow().clone(), || *value.borrow_mut() = None);
        assert!(token.is_none());
        assert!(value.borrow().is_none());

        let clear_calls = Cell::new(0_u8);
        let token = take_activation_token_with(|| None, || clear_calls.set(clear_calls.get() + 1));
        assert!(token.is_none());
        assert_eq!(clear_calls.get(), 0);
    }

    #[test]
    fn truncated_external_launch_requests_are_rejected() {
        let mut short_header = Cursor::new(vec![PROTOCOL_VERSION, EXTERNAL_LAUNCH_MESSAGE, 0]);
        assert_eq!(
            read_external_launch_request(&mut short_header)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );

        let mut short_payload = Cursor::new(vec![
            PROTOCOL_VERSION,
            EXTERNAL_LAUNCH_MESSAGE,
            0,
            4,
            b'a',
            b'b',
        ]);
        assert_eq!(
            read_external_launch_request(&mut short_payload)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn oversized_external_launch_request_is_rejected() {
        let oversized = u16::try_from(MAX_ACTIVATION_TOKEN_BYTES + 1)
            .unwrap()
            .to_be_bytes();
        let mut input = Cursor::new(vec![
            PROTOCOL_VERSION,
            EXTERNAL_LAUNCH_MESSAGE,
            oversized[0],
            oversized[1],
        ]);
        assert_eq!(
            read_external_launch_request(&mut input).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );

        let request = ExternalLaunchRequest {
            activation_token: Some("x".repeat(MAX_ACTIVATION_TOKEN_BYTES + 1)),
        };
        assert_eq!(
            write_external_launch_request(&mut Vec::new(), &request)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn unknown_protocol_version_and_message_type_are_rejected() {
        let mut unknown_version =
            Cursor::new(vec![PROTOCOL_VERSION + 1, EXTERNAL_LAUNCH_MESSAGE, 0, 0]);
        assert_eq!(
            read_external_launch_request(&mut unknown_version)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mut unknown_type = Cursor::new(vec![PROTOCOL_VERSION, 0x7f, 0, 0]);
        assert_eq!(
            read_external_launch_request(&mut unknown_type)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn first_owner_is_primary_second_launch_is_secondary_and_socket_is_private() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert!(matches!(
            claim_without_activation_token(path.clone(), uid).unwrap(),
            SingleInstanceStatus::Secondary
        ));
        drop(primary);
        assert!(!path.exists());
    }

    #[test]
    fn second_client_delivers_external_launch_to_listener() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        let (tx, rx) = mpsc::sync_channel(1);
        primary
            .start_listener(move |request| tx.send(request).is_ok())
            .unwrap();

        assert!(matches!(
            claim_without_activation_token(path, uid).unwrap(),
            SingleInstanceStatus::Secondary
        ));
        rx.recv_timeout(Duration::from_secs(1))
            .expect("external launch was not delivered");
    }

    #[test]
    fn listener_delivers_multiple_sequential_external_launch_requests() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        let (tx, rx) = mpsc::sync_channel(2);
        primary
            .start_listener(move |request| tx.send(request).is_ok())
            .unwrap();

        send_request(&path, &ExternalLaunchRequest::default());
        send_request(
            &path,
            &ExternalLaunchRequest {
                activation_token: Some("second-token".to_owned()),
            },
        );

        let first = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(first.activation_token.is_none());
        assert_eq!(second.activation_token.as_deref(), Some("second-token"));
    }

    #[test]
    fn sequential_secondary_launches_notify_the_same_primary() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        let (tx, rx) = mpsc::sync_channel(2);
        primary
            .start_listener(move |_| tx.send(()).is_ok())
            .unwrap();

        assert!(matches!(
            claim_without_activation_token(path.clone(), uid).unwrap(),
            SingleInstanceStatus::Secondary
        ));
        assert!(matches!(
            claim_without_activation_token(path, uid).unwrap(),
            SingleInstanceStatus::Secondary
        ));
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
        rx.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn stale_socket_is_removed_and_reclaimed() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        assert!(path.exists());

        let primary =
            take_primary(claim_without_activation_token(path.clone(), effective_uid()).unwrap());
        assert!(path.exists());
        drop(primary);
        assert!(!path.exists());
    }

    #[test]
    fn simultaneous_first_launches_never_both_become_primary() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let start = Arc::new(std::sync::Barrier::new(3));
        let release = Arc::new(std::sync::Barrier::new(3));
        let (tx, rx) = mpsc::sync_channel(2);
        let mut workers = Vec::new();

        for _ in 0..2 {
            let path = path.clone();
            let start = Arc::clone(&start);
            let release = Arc::clone(&release);
            let tx = tx.clone();
            workers.push(std::thread::spawn(move || {
                start.wait();
                let status = claim_without_activation_token(path, uid).unwrap();
                let is_primary = matches!(status, SingleInstanceStatus::Primary(_));
                tx.send(is_primary).unwrap();
                release.wait();
                drop(status);
            }));
        }

        start.wait();
        let first = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let second = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_ne!(first, second);
        release.wait();
        for worker in workers {
            worker.join().unwrap();
        }
    }

    #[test]
    fn malformed_requests_do_not_stop_listener() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        let (tx, rx) = mpsc::sync_channel(1);
        primary
            .start_listener(move |request| tx.send(request).is_ok())
            .unwrap();

        let mut unknown = UnixStream::connect(&path).unwrap();
        unknown
            .write_all(&[PROTOCOL_VERSION + 1, EXTERNAL_LAUNCH_MESSAGE, 0, 0])
            .unwrap();
        drop(unknown);

        let mut unknown_type = UnixStream::connect(&path).unwrap();
        unknown_type
            .write_all(&[PROTOCOL_VERSION, 0x7f, 0, 0])
            .unwrap();
        drop(unknown_type);

        let mut truncated = UnixStream::connect(&path).unwrap();
        truncated
            .write_all(&[PROTOCOL_VERSION, EXTERNAL_LAUNCH_MESSAGE, 0])
            .unwrap();
        drop(truncated);

        let oversized = u16::try_from(MAX_ACTIVATION_TOKEN_BYTES + 1)
            .unwrap()
            .to_be_bytes();
        let mut oversized_request = UnixStream::connect(&path).unwrap();
        oversized_request
            .write_all(&[
                PROTOCOL_VERSION,
                EXTERNAL_LAUNCH_MESSAGE,
                oversized[0],
                oversized[1],
            ])
            .unwrap();
        drop(oversized_request);

        send_request(&path, &ExternalLaunchRequest::default());
        let request = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("listener stopped after malformed request");
        assert!(request.activation_token.is_none());
    }

    #[test]
    fn existing_non_socket_is_never_deleted_as_stale() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        fs::write(&path, b"not a socket").unwrap();
        let error = claim_without_activation_token(path.clone(), effective_uid()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"not a socket");
    }

    #[test]
    fn dropping_primary_stops_listener_and_allows_clean_reclaim() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        primary.start_listener(|_| true).unwrap();
        drop(primary);
        assert!(!path.exists());

        let replacement = take_primary(claim_without_activation_token(path, uid).unwrap());
        drop(replacement);
    }
}
