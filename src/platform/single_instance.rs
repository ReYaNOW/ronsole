use crate::launch::TerminalLaunchSpec;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

const SOCKET_FILE_NAME: &str = "instance.sock";
const XDG_ACTIVATION_TOKEN_ENV: &str = "XDG_ACTIVATION_TOKEN";
const PROTOCOL_VERSION: u8 = 2;
const EXTERNAL_LAUNCH_MESSAGE: u8 = 1;
const DELIVERY_ACK_MESSAGE: u8 = 2;
const TAB_INPUT_MESSAGE: u8 = 3;
const TAB_INPUT_RESULT_MESSAGE: u8 = 4;
const REQUEST_HEADER_LEN: usize = 6;
const DELIVERY_ACK_LEN: usize = 2;
const REQUEST_PAYLOAD_PREFIX_LEN: usize = 9;
const REQUEST_FLAG_HOLD: u8 = 1 << 0;
const REQUEST_FLAG_WORKING_DIRECTORY: u8 = 1 << 1;
const REQUEST_FLAG_BRIDGE_LAUNCH: u8 = 1 << 2;
const REQUEST_KNOWN_FLAGS: u8 =
    REQUEST_FLAG_HOLD | REQUEST_FLAG_WORKING_DIRECTORY | REQUEST_FLAG_BRIDGE_LAUNCH;
const MAX_REQUEST_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_ACTIVATION_TOKEN_BYTES: usize = 4096;
const CLAIM_RETRIES: usize = 8;
const SERVER_CLIENT_IO_TIMEOUT: Duration = Duration::from_millis(250);
const SECONDARY_HANDOFF_TIMEOUT: Duration = Duration::from_secs(5);
const TAB_INPUT_IO_TIMEOUT: Duration = Duration::from_secs(1);
const TAB_INPUT_REPLY_LIMIT: usize = 8;
const TAB_INPUT_MIN_DELAY_MS: u16 = 100;
const TAB_INPUT_MAX_DELAY_MS: u16 = 2000;
const TAB_INPUT_MAX_LINE_BYTES: usize = 4096;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TabInputRequest {
    pub(crate) token: String,
    pub(crate) delay_ms: u16,
    pub(crate) kind: TabInputKind,
    pub(crate) op: [u8; 16],
    pub(crate) line: String,
}

impl std::fmt::Debug for TabInputRequest {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TabInputRequest")
            .field("token", &"<redacted>")
            .field("delay_ms", &self.delay_ms)
            .field("kind", &self.kind)
            .field("op", &self.op)
            .field("line", &self.line)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabInputKind {
    Line,
    Clear,
    PhraseAfterClear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PeerCred {
    pub(crate) uid: u32,
    pub(crate) pid: u32,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabInputCode {
    InputDelivered = 0,
    NoTab = 1,
    LeaderNotClaude = 2,
    PeerRejected = 3,
    InvalidInput = 4,
    RateLimitedOrQueued = 5,
    InputBoxUnavailable = 6,
    RecentUserInput = 7,
    LeaderChanged = 8,
    PtyWriteFailed = 9,
    ClearCauseMissing = 10,
}

impl TabInputCode {
    const ALL: [Self; 11] = [
        Self::InputDelivered, Self::NoTab, Self::LeaderNotClaude, Self::PeerRejected,
        Self::InvalidInput, Self::RateLimitedOrQueued, Self::InputBoxUnavailable,
        Self::RecentUserInput, Self::LeaderChanged, Self::PtyWriteFailed, Self::ClearCauseMissing,
    ];
}

#[derive(Debug)]
pub(crate) struct TabInputEnvelope {
    pub(crate) request: TabInputRequest,
    pub(crate) peer: PeerCred,
    pub(crate) reply: mpsc::Sender<TabInputCode>,
}

#[derive(Debug)]
pub(crate) enum InstanceMessage {
    ExternalLaunch(ExternalLaunchRequest),
    TabInput(TabInputEnvelope),
}

pub(crate) fn peer_allowed(peer: PeerCred, own_uid: u32) -> bool {
    peer.uid == own_uid
}

pub(crate) fn validate_tab_input_text(kind: TabInputKind, line: &str) -> bool {
    if line.is_empty() || line.len() > TAB_INPUT_MAX_LINE_BYTES {
        return false;
    }
    if line.chars().any(|character| {
        character.is_control()
            || matches!(
                character as u32,
                0x00ad
                    | 0x200b..=0x200f
                    | 0x2028..=0x202e
                    | 0x2060..=0x2064
                    | 0x2066..=0x2069
                    | 0xfeff
                    | 0xfff9..=0xfffb
            )
    }) {
        return false;
    }
    match kind {
        TabInputKind::Line => line.starts_with("/effort "),
        TabInputKind::Clear => line == "/clear",
        TabInputKind::PhraseAfterClear => {
            let trimmed = line.trim();
            !trimmed.is_empty()
                && !trimmed.chars().next().is_some_and(|character| {
                    matches!(character, '/' | '!' | '#' | '@' | '&')
                })
        }
    }
}

fn reserve_tab_input_reply(active: &AtomicUsize) -> bool {
    let mut count = active.load(Ordering::Acquire);
    loop {
        if count >= TAB_INPUT_REPLY_LIMIT {
            return false;
        }
        match active.compare_exchange_weak(count, count + 1, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(actual) => count = actual,
        }
    }
}

fn release_tab_input_reply(active: &AtomicUsize) {
    active.fetch_sub(1, Ordering::AcqRel);
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct ExternalLaunchRequest {
    pub(crate) activation_token: Option<String>,
    pub(crate) launch: TerminalLaunchSpec,
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
    pub(crate) fn start_listener<F>(&mut self, mut deliver: F) -> io::Result<()>
    where
        F: FnMut(InstanceMessage) -> bool + Send + 'static,
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
        let active_replies = Arc::new(AtomicUsize::new(0));
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
                let Ok(peer) = peer_credentials(&stream) else { continue };
                let _ = stream.set_read_timeout(Some(SERVER_CLIENT_IO_TIMEOUT));
                let _ = stream.set_write_timeout(Some(SERVER_CLIENT_IO_TIMEOUT));
                let mut prefix = [0_u8; 2];
                if stream.read_exact(&mut prefix).is_err() {
                    continue;
                }
                if prefix[1] == TAB_INPUT_MESSAGE && !peer_allowed(peer, effective_uid()) {
                    let _ = write_tab_input_result(&mut stream, TabInputCode::PeerRejected);
                    continue;
                }
                let mut framed = io::Cursor::new(prefix).chain(&mut stream);
                let message = read_instance_message(&mut framed, peer);
                drop(framed);
                match message {
                    Ok(InstanceMessage::ExternalLaunch(request)) => {
                        if deliver(InstanceMessage::ExternalLaunch(request)) {
                            let _ = write_delivery_ack(&mut stream);
                        }
                    }
                    Ok(InstanceMessage::TabInput(mut envelope)) => {
                        if !reserve_tab_input_reply(&active_replies) {
                            let _ = write_tab_input_result(&mut stream, TabInputCode::RateLimitedOrQueued);
                            continue;
                        }
                        let (reply_tx, reply_rx) = mpsc::channel();
                        envelope.reply = reply_tx;
                        let delay_ms = envelope.request.delay_ms;
                        if !deliver(InstanceMessage::TabInput(envelope)) {
                            release_tab_input_reply(&active_replies);
                            let _ = write_tab_input_result(&mut stream, TabInputCode::RateLimitedOrQueued);
                            continue;
                        }
                        let _ = stream.set_read_timeout(Some(TAB_INPUT_IO_TIMEOUT));
                        let _ = stream.set_write_timeout(Some(TAB_INPUT_IO_TIMEOUT));
                        let active_for_thread = Arc::clone(&active_replies);
                        let spawned = std::thread::Builder::new()
                            .name("ronsole-tab-input-reply".to_owned())
                            .spawn(move || {
                                let response = reply_rx.recv_timeout(
                                    Duration::from_millis(u64::from(delay_ms)) + Duration::from_secs(1),
                                );
                                if let Ok(code) = response {
                                    let _ = write_tab_input_result(&mut stream, code);
                                }
                                release_tab_input_reply(&active_for_thread);
                            });
                        if spawned.is_err() {
                            release_tab_input_reply(&active_replies);
                            // The reply worker could not be created; the stream is closed below.
                        }
                    }
                    Err(_) => {}
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

pub(crate) fn acquire_single_instance(
    initial_launch: &TerminalLaunchSpec,
) -> io::Result<SingleInstanceStatus> {
    let uid = effective_uid();
    if let Some(preferred) = preferred_runtime_dir(std::env::var_os("XDG_RUNTIME_DIR"))
        && prepare_private_runtime_dir(&preferred, uid).is_ok()
    {
        return claim_at(preferred.join(SOCKET_FILE_NAME), uid, initial_launch);
    }

    let fallback = fallback_runtime_dir(uid);
    prepare_private_runtime_dir(&fallback, uid)?;
    claim_at(fallback.join(SOCKET_FILE_NAME), uid, initial_launch)
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

fn claim_at(
    socket_path: PathBuf,
    uid: u32,
    initial_launch: &TerminalLaunchSpec,
) -> io::Result<SingleInstanceStatus> {
    claim_at_with_request(socket_path, uid, || ExternalLaunchRequest {
        activation_token: take_activation_token(),
        launch: initial_launch.clone(),
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
    stream.set_read_timeout(Some(SECONDARY_HANDOFF_TIMEOUT))?;
    stream.set_write_timeout(Some(SECONDARY_HANDOFF_TIMEOUT))?;
    let request = request();
    write_external_launch_request(&mut stream, &request)?;
    read_delivery_ack(&mut stream)
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

fn request_payload_len(request: &ExternalLaunchRequest) -> io::Result<usize> {
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
    u16::try_from(token.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "single-instance activation token exceeds protocol framing limit",
        )
    })?;

    let working_directory = request
        .launch
        .working_directory
        .as_deref()
        .map(|path| path.as_os_str().as_bytes())
        .unwrap_or_default();
    u32::try_from(working_directory.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "single-instance working directory exceeds protocol framing limit",
        )
    })?;
    u16::try_from(request.launch.command.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "single-instance command has too many arguments",
        )
    })?;

    let mut payload_len = REQUEST_PAYLOAD_PREFIX_LEN;
    payload_len = payload_len
        .checked_add(token.len())
        .and_then(|len| len.checked_add(working_directory.len()))
        .and_then(|len| {
            len.checked_add(if request.launch.bridge_launch_id.is_some() {
                16
            } else {
                0
            })
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "single-instance request size overflow",
            )
        })?;
    for arg in &request.launch.command {
        let bytes = arg.as_os_str().as_bytes();
        u32::try_from(bytes.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "single-instance command argument exceeds protocol framing limit",
            )
        })?;
        payload_len = payload_len
            .checked_add(std::mem::size_of::<u32>())
            .and_then(|len| len.checked_add(bytes.len()))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "single-instance request size overflow",
                )
            })?;
    }
    if payload_len > MAX_REQUEST_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "single-instance request exceeds protocol limit",
        ));
    }
    Ok(payload_len)
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
    let working_directory = request
        .launch
        .working_directory
        .as_deref()
        .map(|path| path.as_os_str().as_bytes())
        .unwrap_or_default();
    let payload_len = u32::try_from(request_payload_len(request)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "single-instance request exceeds protocol framing limit",
        )
    })?;
    let mut flags = 0_u8;
    if request.launch.hold {
        flags |= REQUEST_FLAG_HOLD;
    }
    if request.launch.working_directory.is_some() {
        flags |= REQUEST_FLAG_WORKING_DIRECTORY;
    }
    if request.launch.bridge_launch_id.is_some() {
        flags |= REQUEST_FLAG_BRIDGE_LAUNCH;
    }

    writer.write_all(&[PROTOCOL_VERSION, EXTERNAL_LAUNCH_MESSAGE])?;
    writer.write_all(&payload_len.to_be_bytes())?;
    writer.write_all(&[flags])?;
    writer.write_all(
        &u16::try_from(token.len())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "single-instance activation token exceeds protocol framing limit",
                )
            })?
            .to_be_bytes(),
    )?;
    writer.write_all(
        &u32::try_from(working_directory.len())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "single-instance working directory exceeds protocol framing limit",
                )
            })?
            .to_be_bytes(),
    )?;
    writer.write_all(
        &u16::try_from(request.launch.command.len())
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "single-instance command has too many arguments",
                )
            })?
            .to_be_bytes(),
    )?;
    writer.write_all(token)?;
    writer.write_all(working_directory)?;
    for arg in &request.launch.command {
        let bytes = arg.as_os_str().as_bytes();
        writer.write_all(
            &u32::try_from(bytes.len())
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "single-instance command argument exceeds protocol framing limit",
                    )
                })?
                .to_be_bytes(),
        )?;
        writer.write_all(bytes)?;
    }
    if let Some(bridge_launch_id) = request.launch.bridge_launch_id {
        writer.write_all(&bridge_launch_id)?;
    }
    Ok(())
}

fn take_payload_bytes<'a>(payload: &mut &'a [u8], len: usize) -> io::Result<&'a [u8]> {
    if payload.len() < len {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated single-instance request payload",
        ));
    }
    let (value, remaining) = payload.split_at(len);
    *payload = remaining;
    Ok(value)
}

fn take_payload_u8(payload: &mut &[u8]) -> io::Result<u8> {
    Ok(take_payload_bytes(payload, 1)?[0])
}

fn take_payload_u16(payload: &mut &[u8]) -> io::Result<u16> {
    let bytes = take_payload_bytes(payload, std::mem::size_of::<u16>())?;
    Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
}

fn take_payload_u32(payload: &mut &[u8]) -> io::Result<u32> {
    let bytes = take_payload_bytes(payload, std::mem::size_of::<u32>())?;
    Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
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

    let payload_len = usize::try_from(u32::from_be_bytes([
        header[2], header[3], header[4], header[5],
    ]))
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "single-instance request length exceeds platform limits",
        )
    })?;
    if payload_len > MAX_REQUEST_PAYLOAD_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "single-instance request exceeds protocol limit",
        ));
    }

    let mut payload_storage = vec![0_u8; payload_len];
    reader.read_exact(&mut payload_storage)?;
    let mut payload = payload_storage.as_slice();
    let flags = take_payload_u8(&mut payload)?;
    if flags & !REQUEST_KNOWN_FLAGS != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "single-instance request contains unknown flags",
        ));
    }
    let token_len = usize::from(take_payload_u16(&mut payload)?);
    if token_len > MAX_ACTIVATION_TOKEN_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "single-instance activation token exceeds protocol limit",
        ));
    }
    let working_directory_len = usize::try_from(take_payload_u32(&mut payload)?).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "single-instance working directory length exceeds platform limits",
        )
    })?;
    let command_len = usize::from(take_payload_u16(&mut payload)?);

    let token = take_payload_bytes(&mut payload, token_len)?;
    let working_directory = take_payload_bytes(&mut payload, working_directory_len)?;
    if flags & REQUEST_FLAG_WORKING_DIRECTORY == 0 && !working_directory.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "single-instance request has unflagged working directory bytes",
        ));
    }
    let minimum_command_framing = command_len
        .checked_mul(std::mem::size_of::<u32>())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "single-instance command framing size overflow",
            )
        })?;
    if minimum_command_framing > payload.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated single-instance command framing",
        ));
    }

    let activation_token = if token.is_empty() {
        None
    } else {
        Some(
            std::str::from_utf8(token)
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "single-instance activation token is not valid UTF-8",
                    )
                })?
                .to_owned(),
        )
    };
    let working_directory = (flags & REQUEST_FLAG_WORKING_DIRECTORY != 0)
        .then(|| PathBuf::from(OsString::from_vec(working_directory.to_vec())));
    let mut command = Vec::with_capacity(command_len);
    for _ in 0..command_len {
        let arg_len = usize::try_from(take_payload_u32(&mut payload)?).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "single-instance command argument length exceeds platform limits",
            )
        })?;
        let arg = take_payload_bytes(&mut payload, arg_len)?;
        command.push(OsString::from_vec(arg.to_vec()));
    }
    let bridge_launch_id = if flags & REQUEST_FLAG_BRIDGE_LAUNCH != 0 {
        let bytes = take_payload_bytes(&mut payload, 16)?;
        Some(<[u8; 16]>::try_from(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid bridge launch id length",
            )
        })?)
    } else {
        None
    };
    if !payload.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "single-instance request contains trailing payload bytes",
        ));
    }

    Ok(ExternalLaunchRequest {
        activation_token,
        launch: TerminalLaunchSpec {
            working_directory,
            command,
            hold: flags & REQUEST_FLAG_HOLD != 0,
            bridge_launch_id,
        },
    })
}

fn write_delivery_ack(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(&[PROTOCOL_VERSION, DELIVERY_ACK_MESSAGE])
}

fn read_delivery_ack(reader: &mut impl Read) -> io::Result<()> {
    let mut ack = [0_u8; DELIVERY_ACK_LEN];
    reader.read_exact(&mut ack)?;
    if ack[0] != PROTOCOL_VERSION || ack[1] != DELIVERY_ACK_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid single-instance delivery acknowledgement",
        ));
    }
    Ok(())
}

fn peer_credentials(stream: &UnixStream) -> io::Result<PeerCred> {
    let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt initializes the ucred structure when it succeeds.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            credentials.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    if usize::try_from(length).ok() != Some(std::mem::size_of::<libc::ucred>()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid SO_PEERCRED size"));
    }
    // SAFETY: successful getsockopt with the expected length initialized the structure.
    let credentials = unsafe { credentials.assume_init() };
    Ok(PeerCred { uid: credentials.uid, pid: u32::try_from(credentials.pid).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "invalid peer process id")
    })? })
}

fn read_instance_message(reader: &mut impl Read, peer: PeerCred) -> io::Result<InstanceMessage> {
    let mut prefix = [0_u8; 2];
    reader.read_exact(&mut prefix)?;
    let mut framed = io::Cursor::new(prefix).chain(reader);
    match prefix[1] {
        EXTERNAL_LAUNCH_MESSAGE => read_external_launch_request(&mut framed)
            .map(InstanceMessage::ExternalLaunch),
        TAB_INPUT_MESSAGE => {
            let request = read_tab_input_request(&mut framed)?;
            let (reply, _receiver) = mpsc::channel();
            Ok(InstanceMessage::TabInput(TabInputEnvelope { request, peer, reply }))
        }
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "unsupported single-instance message type")),
    }
}

fn write_tab_input_request(writer: &mut impl Write, request: &TabInputRequest) -> io::Result<()> {
    let token = request.token.as_bytes();
    let line = request.line.as_bytes();
    if token.is_empty() || token.len() > 4096 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid tab token length"));
    }
    if !(TAB_INPUT_MIN_DELAY_MS..=TAB_INPUT_MAX_DELAY_MS).contains(&request.delay_ms) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid tab input delay"));
    }
    if !(1..=TAB_INPUT_MAX_LINE_BYTES).contains(&line.len()) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid tab input line length"));
    }
    let payload_len = 2 + token.len() + 2 + 1 + request.op.len() + 2 + line.len();
    writer.write_all(&[PROTOCOL_VERSION, TAB_INPUT_MESSAGE])?;
    writer.write_all(&u32::try_from(payload_len).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "tab input frame too large"))?.to_be_bytes())?;
    writer.write_all(&u16::try_from(token.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "tab token too long"))?.to_be_bytes())?;
    writer.write_all(token)?;
    writer.write_all(&request.delay_ms.to_be_bytes())?;
    writer.write_all(&[request.kind as u8])?;
    writer.write_all(&request.op)?;
    writer.write_all(&u16::try_from(line.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "tab input line too long"))?.to_be_bytes())?;
    writer.write_all(line)
}

fn read_tab_input_request(reader: &mut impl Read) -> io::Result<TabInputRequest> {
    let mut header = [0_u8; REQUEST_HEADER_LEN];
    reader.read_exact(&mut header)?;
    if header[0] != PROTOCOL_VERSION || header[1] != TAB_INPUT_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid tab input frame header"));
    }
    let payload_len = usize::try_from(u32::from_be_bytes(header[2..6].try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid tab input frame length"))?))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "tab input frame length out of range"))?;
    if payload_len > 2 + 4096 + 2 + 1 + 16 + 2 + TAB_INPUT_MAX_LINE_BYTES {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "tab input frame too large"));
    }
    let mut payload = vec![0; payload_len];
    reader.read_exact(&mut payload)?;
    let mut payload = payload.as_slice();
    let token_len = usize::from(take_payload_u16(&mut payload)?);
    if !(1..=4096).contains(&token_len) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid tab token length"));
    }
    let token = std::str::from_utf8(take_payload_bytes(&mut payload, token_len)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "tab token is not UTF-8"))?.to_owned();
    let delay_ms = take_payload_u16(&mut payload)?;
    if !(TAB_INPUT_MIN_DELAY_MS..=TAB_INPUT_MAX_DELAY_MS).contains(&delay_ms) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid tab input delay"));
    }
    let kind = match take_payload_u8(&mut payload)? {
        0 => TabInputKind::Line,
        1 => TabInputKind::Clear,
        2 => TabInputKind::PhraseAfterClear,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown tab input kind")),
    };
    let mut op = [0; 16];
    let op_bytes = take_payload_bytes(&mut payload, op.len())?;
    op.copy_from_slice(op_bytes);
    let line_len = usize::from(take_payload_u16(&mut payload)?);
    if !(1..=TAB_INPUT_MAX_LINE_BYTES).contains(&line_len) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid tab input line length"));
    }
    let line = std::str::from_utf8(take_payload_bytes(&mut payload, line_len)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "tab input line is not UTF-8"))?.to_owned();
    if !payload.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "tab input frame contains trailing bytes"));
    }
    Ok(TabInputRequest { token, delay_ms, kind, op, line })
}

fn write_tab_input_result(writer: &mut impl Write, code: TabInputCode) -> io::Result<()> {
    writer.write_all(&[PROTOCOL_VERSION, TAB_INPUT_RESULT_MESSAGE, code as u8])
}

fn read_tab_input_result(reader: &mut impl Read) -> io::Result<TabInputCode> {
    let mut result = [0_u8; 3];
    reader.read_exact(&mut result)?;
    if result[0] != PROTOCOL_VERSION || result[1] != TAB_INPUT_RESULT_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid tab input result header"));
    }
    TabInputCode::ALL.get(usize::from(result[2])).copied()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown tab input result code"))
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
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
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

    fn external_message(message: InstanceMessage) -> ExternalLaunchRequest {
        match message {
            InstanceMessage::ExternalLaunch(request) => request,
            InstanceMessage::TabInput(_) => panic!("unexpected tab input"),
        }
    }

    fn claim_without_activation_token(
        socket_path: PathBuf,
        uid: u32,
    ) -> io::Result<SingleInstanceStatus> {
        claim_at_with_request(socket_path, uid, ExternalLaunchRequest::default)
    }

    fn claim_with_request(
        socket_path: PathBuf,
        uid: u32,
        request: &ExternalLaunchRequest,
    ) -> io::Result<SingleInstanceStatus> {
        let request = request.clone();
        claim_at_with_request(socket_path, uid, move || request.clone())
    }

    fn round_trip_request(request: &ExternalLaunchRequest) -> io::Result<ExternalLaunchRequest> {
        let mut encoded = Vec::new();
        write_external_launch_request(&mut encoded, request)?;
        read_external_launch_request(&mut Cursor::new(encoded))
    }

    fn send_request(path: &Path, request: &ExternalLaunchRequest) {
        let request = request.clone();
        let mut request_factory = move || request.clone();
        notify_existing(path, &mut request_factory).unwrap();
    }

    fn request_header(version: u8, message: u8, payload_len: u32) -> Vec<u8> {
        let mut header = vec![version, message];
        header.extend_from_slice(&payload_len.to_be_bytes());
        header
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
    fn external_launch_request_round_trips_default_request() {
        let request = ExternalLaunchRequest::default();
        let mut encoded = Vec::new();
        write_external_launch_request(&mut encoded, &request).unwrap();
        assert_eq!(encoded[REQUEST_HEADER_LEN], 0);
        assert_eq!(
            read_external_launch_request(&mut Cursor::new(encoded)).unwrap(),
            request
        );
    }

    #[test]
    fn external_launch_request_round_trips_token() {
        let request = ExternalLaunchRequest {
            activation_token: Some("wayland-token-123".to_owned()),
            launch: TerminalLaunchSpec::default(),
        };
        assert_eq!(round_trip_request(&request).unwrap(), request);
    }

    #[test]
    fn external_launch_request_round_trips_full_launch_spec() {
        let request = ExternalLaunchRequest {
            activation_token: Some("wayland-token-123".to_owned()),
            launch: TerminalLaunchSpec {
                working_directory: Some(PathBuf::from("/tmp/ronsole session")),
                command: vec![
                    OsString::from("program"),
                    OsString::from("arg one"),
                    OsString::from("--leading-dash"),
                    OsString::from("русский/utf8"),
                ],
                hold: true,
                bridge_launch_id: Some([0x11; 16]),
            },
        };
        assert_eq!(round_trip_request(&request).unwrap(), request);
    }

    #[test]
    fn external_launch_request_round_trips_non_utf8_workdir_and_argv() {
        let raw_workdir = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]);
        let raw_program = OsString::from_vec(vec![b'f', b'o', b'o', 0xfe]);
        let raw_arg = OsString::from_vec(vec![b'a', b'r', b'g', 0xfd]);
        let request = ExternalLaunchRequest {
            activation_token: None,
            launch: TerminalLaunchSpec {
                working_directory: Some(PathBuf::from(raw_workdir.clone())),
                command: vec![raw_program.clone(), raw_arg.clone()],
                hold: false,
                bridge_launch_id: None,
            },
        };
        let decoded = round_trip_request(&request).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(
            decoded
                .launch
                .working_directory
                .as_deref()
                .unwrap()
                .as_os_str()
                .as_bytes(),
            raw_workdir.as_os_str().as_bytes()
        );
        assert_eq!(
            decoded.launch.command[0].as_os_str().as_bytes(),
            raw_program.as_os_str().as_bytes()
        );
        assert_eq!(
            decoded.launch.command[1].as_os_str().as_bytes(),
            raw_arg.as_os_str().as_bytes()
        );
    }

    #[test]
    fn external_launch_request_debug_redacts_activation_token() {
        let request = ExternalLaunchRequest {
            activation_token: Some("secret-activation-token".to_owned()),
            launch: TerminalLaunchSpec {
                working_directory: Some(PathBuf::from("/secret/cwd")),
                command: vec![OsString::from("secret-command")],
                hold: true,
                bridge_launch_id: None,
            },
        };
        let debug = format!("{request:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-activation-token"));
        assert!(!debug.contains("secret-command"));
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

        let mut short_payload = request_header(
            PROTOCOL_VERSION,
            EXTERNAL_LAUNCH_MESSAGE,
            u32::try_from(REQUEST_PAYLOAD_PREFIX_LEN).unwrap(),
        );
        short_payload.extend_from_slice(&[0, 0]);
        assert_eq!(
            read_external_launch_request(&mut Cursor::new(short_payload))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );

        let mut missing_bridge_id = Vec::new();
        write_external_launch_request(&mut missing_bridge_id, &ExternalLaunchRequest::default())
            .unwrap();
        missing_bridge_id[REQUEST_HEADER_LEN] |= REQUEST_FLAG_BRIDGE_LAUNCH;
        assert_eq!(
            read_external_launch_request(&mut Cursor::new(missing_bridge_id))
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn oversized_request_is_rejected_before_payload_read() {
        let oversized = u32::try_from(MAX_REQUEST_PAYLOAD_BYTES + 1).unwrap();
        let input = request_header(PROTOCOL_VERSION, EXTERNAL_LAUNCH_MESSAGE, oversized);
        assert_eq!(
            read_external_launch_request(&mut Cursor::new(input))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let request = ExternalLaunchRequest {
            activation_token: None,
            launch: TerminalLaunchSpec {
                working_directory: None,
                command: vec![OsString::from_vec(vec![b'x'; MAX_REQUEST_PAYLOAD_BYTES])],
                hold: false,
                bridge_launch_id: None,
            },
        };
        assert_eq!(
            write_external_launch_request(&mut Vec::new(), &request)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn oversized_activation_token_is_rejected() {
        let mut payload = vec![0_u8];
        payload.extend_from_slice(
            &u16::try_from(MAX_ACTIVATION_TOKEN_BYTES + 1)
                .unwrap()
                .to_be_bytes(),
        );
        payload.extend_from_slice(&0_u32.to_be_bytes());
        payload.extend_from_slice(&0_u16.to_be_bytes());
        let mut input = request_header(
            PROTOCOL_VERSION,
            EXTERNAL_LAUNCH_MESSAGE,
            u32::try_from(payload.len()).unwrap(),
        );
        input.extend_from_slice(&payload);
        assert_eq!(
            read_external_launch_request(&mut Cursor::new(input))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let request = ExternalLaunchRequest {
            activation_token: Some("x".repeat(MAX_ACTIVATION_TOKEN_BYTES + 1)),
            launch: TerminalLaunchSpec::default(),
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
        let mut unknown_version = Cursor::new(request_header(
            PROTOCOL_VERSION + 1,
            EXTERNAL_LAUNCH_MESSAGE,
            0,
        ));
        assert_eq!(
            read_external_launch_request(&mut unknown_version)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );

        let mut unknown_type = Cursor::new(request_header(PROTOCOL_VERSION, 0x7f, 0));
        assert_eq!(
            read_external_launch_request(&mut unknown_type)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn delivery_ack_round_trips_and_rejects_invalid_frames() {
        let mut encoded = Vec::new();
        write_delivery_ack(&mut encoded).unwrap();
        read_delivery_ack(&mut Cursor::new(encoded)).unwrap();

        let mut wrong_version = Cursor::new(vec![PROTOCOL_VERSION + 1, DELIVERY_ACK_MESSAGE]);
        assert_eq!(
            read_delivery_ack(&mut wrong_version).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut wrong_message = Cursor::new(vec![PROTOCOL_VERSION, EXTERNAL_LAUNCH_MESSAGE]);
        assert_eq!(
            read_delivery_ack(&mut wrong_message).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    fn sample_tab_input(kind: TabInputKind, line: &str) -> TabInputRequest {
        TabInputRequest {
            token: "0123456789abcdef0123456789abcdef".to_owned(),
            delay_ms: 800,
            kind,
            op: [7; 16],
            line: line.to_owned(),
        }
    }

    #[test]
    fn tab_input_request_and_result_round_trip_all_kinds() {
        for (kind, line) in [
            (TabInputKind::Line, "/effort high"),
            (TabInputKind::Clear, "/clear"),
            (TabInputKind::PhraseAfterClear, "continue with the plan"),
        ] {
            let request = sample_tab_input(kind, line);
            let mut encoded = Vec::new();
            write_tab_input_request(&mut encoded, &request).unwrap();
            assert_eq!(read_tab_input_request(&mut Cursor::new(encoded)).unwrap(), request);
        }
        for code in TabInputCode::ALL {
            let mut encoded = Vec::new();
            write_tab_input_result(&mut encoded, code).unwrap();
            assert_eq!(encoded, [2, 4, code as u8]);
            assert_eq!(read_tab_input_result(&mut Cursor::new(encoded)).unwrap(), code);
        }
    }

    #[test]
    fn tab_input_rejects_invalid_frames() {
        let valid = |request: &TabInputRequest| {
            let mut bytes = Vec::new();
            write_tab_input_request(&mut bytes, request).unwrap();
            bytes
        };
        let base = sample_tab_input(TabInputKind::Line, "/effort high");
        let encoded = valid(&base);
        let delay_offset = REQUEST_HEADER_LEN + 2 + base.token.len();
        let line_length_offset = delay_offset + 2 + 1 + base.op.len();
        for delay in [99_u16, 2001] {
            let mut invalid = encoded.clone();
            invalid[delay_offset..delay_offset + 2].copy_from_slice(&delay.to_be_bytes());
            assert!(read_tab_input_request(&mut Cursor::new(invalid)).is_err());
        }
        for line_len in [0_u16, 4097] {
            let mut invalid = encoded.clone();
            invalid[line_length_offset..line_length_offset + 2]
                .copy_from_slice(&line_len.to_be_bytes());
            assert!(read_tab_input_request(&mut Cursor::new(invalid)).is_err());
        }
        let mut unknown_kind = valid(&base);
        let kind_offset = 6 + 2 + base.token.len() + 2;
        unknown_kind[kind_offset] = 3;
        assert!(read_tab_input_request(&mut Cursor::new(unknown_kind)).is_err());
        let mut invalid_utf8 = valid(&base);
        *invalid_utf8.last_mut().unwrap() = 0xff;
        assert!(read_tab_input_request(&mut Cursor::new(invalid_utf8)).is_err());
        let mut trailing = valid(&base);
        trailing.push(0);
        let payload_len = u32::from_be_bytes(trailing[2..6].try_into().unwrap()) + 1;
        trailing[2..6].copy_from_slice(&payload_len.to_be_bytes());
        assert!(read_tab_input_request(&mut Cursor::new(trailing)).is_err());
        let mut truncated = valid(&base);
        truncated.pop();
        assert!(read_tab_input_request(&mut Cursor::new(truncated)).is_err());
    }

    #[test]
    fn tab_input_text_validation_enforces_kind_and_unicode_rules() {
        for line in ["hello\rworld", "hello\u{1b}world", "hello\u{7f}", "a\u{202e}b", "a\u{2066}b"] {
            assert!(!validate_tab_input_text(TabInputKind::PhraseAfterClear, line));
        }
        assert!(!validate_tab_input_text(TabInputKind::Line, "\u{feff}/effort low"));
        assert!(!validate_tab_input_text(TabInputKind::PhraseAfterClear, "\u{200d}x"));
        assert!(!validate_tab_input_text(TabInputKind::Clear, "/clear now"));
        assert!(!validate_tab_input_text(TabInputKind::Line, "hello"));
        assert!(!validate_tab_input_text(TabInputKind::PhraseAfterClear, " /x"));
        assert!(validate_tab_input_text(TabInputKind::PhraseAfterClear, " text /x"));
        assert!(validate_tab_input_text(TabInputKind::Line, "/effort high"));
        assert!(validate_tab_input_text(TabInputKind::Clear, "/clear"));
        assert!(validate_tab_input_text(TabInputKind::PhraseAfterClear, "continue"));
    }

    #[test]
    fn peer_allowed_rejects_different_uid() {
        assert!(!peer_allowed(PeerCred { uid: 1001, pid: 7 }, 1000));
        assert!(peer_allowed(PeerCred { uid: 1000, pid: 7 }, 1000));
    }

    #[test]
    fn tab_input_request_debug_redacts_token() {
        let request = sample_tab_input(TabInputKind::Line, "/effort high");
        assert!(!format!("{:?}", request).contains(&request.token));
    }

    #[test]
    fn listener_ninth_waiting_tab_input_gets_overloaded_result_and_releases_slot() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), effective_uid()).unwrap());
        let (tx, rx) = mpsc::channel();
        primary.start_listener(move |message| tx.send(message).is_ok()).unwrap();

        let mut clients = Vec::new();
        let mut envelopes = Vec::new();
        for _ in 0..TAB_INPUT_REPLY_LIMIT {
            let mut stream = UnixStream::connect(&path).unwrap();
            write_tab_input_request(
                &mut stream,
                &sample_tab_input(TabInputKind::Line, "/effort high"),
            )
            .unwrap();
            let envelope = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
                InstanceMessage::TabInput(envelope) => envelope,
                InstanceMessage::ExternalLaunch(_) => panic!("unexpected external launch"),
            };
            clients.push(stream);
            envelopes.push(envelope);
        }

        let mut ninth = UnixStream::connect(&path).unwrap();
        write_tab_input_request(
            &mut ninth,
            &sample_tab_input(TabInputKind::Line, "/effort high"),
        )
        .unwrap();
        let mut result = [0; 3];
        ninth.read_exact(&mut result).unwrap();
        assert_eq!(result, [2, 4, TabInputCode::RateLimitedOrQueued as u8]);

        for envelope in envelopes {
            envelope.reply.send(TabInputCode::InputDelivered).unwrap();
        }
        for client in &mut clients {
            let mut result = [0; 3];
            client.read_exact(&mut result).unwrap();
            assert_eq!(result, [2, 4, TabInputCode::InputDelivered as u8]);
        }

        let mut released = UnixStream::connect(&path).unwrap();
        write_tab_input_request(
            &mut released,
            &sample_tab_input(TabInputKind::Line, "/effort high"),
        )
        .unwrap();
        let envelope = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            InstanceMessage::TabInput(envelope) => envelope,
            InstanceMessage::ExternalLaunch(_) => panic!("unexpected external launch"),
        };
        envelope.reply.send(TabInputCode::InputDelivered).unwrap();
        released.read_exact(&mut result).unwrap();
        assert_eq!(result, [2, 4, TabInputCode::InputDelivered as u8]);
    }

    #[test]
    fn listener_delivers_tab_input_peer_and_reply_while_serving_external_launch() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), effective_uid()).unwrap());
        let (tx, rx) = mpsc::channel();
        primary.start_listener(move |message| tx.send(message).is_ok()).unwrap();

        let request = sample_tab_input(TabInputKind::PhraseAfterClear, "continue");
        let mut stream = UnixStream::connect(&path).unwrap();
        write_tab_input_request(&mut stream, &request).unwrap();
        let envelope = match rx.recv_timeout(Duration::from_secs(1)).unwrap() {
            InstanceMessage::TabInput(envelope) => envelope,
            InstanceMessage::ExternalLaunch(_) => panic!("unexpected external launch"),
        };
        assert_eq!(envelope.request, request);
        assert_eq!(envelope.peer.uid, effective_uid());
        assert_eq!(envelope.peer.pid, std::process::id());

        let external = ExternalLaunchRequest::default();
        let mut external_stream = UnixStream::connect(&path).unwrap();
        write_external_launch_request(&mut external_stream, &external).unwrap();
        assert!(matches!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), InstanceMessage::ExternalLaunch(_)));
        read_delivery_ack(&mut external_stream).unwrap();
        envelope.reply.send(TabInputCode::InputDelivered).unwrap();
        let mut result = [0; 3];
        stream.read_exact(&mut result).unwrap();
        assert_eq!(result, [2, 4, TabInputCode::InputDelivered as u8]);
    }

    #[test]
    fn listener_closes_tab_input_when_callback_does_not_reply() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), effective_uid()).unwrap());
        let (tx, rx) = mpsc::channel();
        primary.start_listener(move |message| { tx.send(message).is_ok() }).unwrap();
        let mut request = sample_tab_input(TabInputKind::Line, "/effort low");
        request.delay_ms = 2000;
        let started = std::time::Instant::now();
        let mut stream = UnixStream::connect(&path).unwrap();
        write_tab_input_request(&mut stream, &request).unwrap();
        let envelope = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(envelope);
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        assert!(response.is_empty());
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn peer_close_without_ack_is_not_successful_delivery() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let listener = UnixListener::bind(&path).unwrap();
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_external_launch_request(&mut stream).unwrap();
            assert_eq!(request, ExternalLaunchRequest::default());
        });

        let request = ExternalLaunchRequest::default();
        let mut request_factory = || request.clone();
        assert_eq!(
            notify_existing(&path, &mut request_factory)
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        worker.join().unwrap();
    }

    #[test]
    fn valid_request_waits_for_delayed_listener_and_is_successful_only_after_ack() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let listener = UnixListener::bind(&path).unwrap();
        let startup_delay = Duration::from_millis(400);
        assert!(startup_delay > SERVER_CLIENT_IO_TIMEOUT);
        assert!(startup_delay < SECONDARY_HANDOFF_TIMEOUT);
        let expected = ExternalLaunchRequest {
            activation_token: Some("token".to_owned()),
            launch: TerminalLaunchSpec {
                working_directory: Some(PathBuf::from("/tmp")),
                command: vec![OsString::from("htop")],
                hold: true,
                bridge_launch_id: None,
            },
        };
        let worker_expected = expected.clone();
        let worker = std::thread::spawn(move || {
            std::thread::sleep(startup_delay);
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_external_launch_request(&mut stream).unwrap();
            assert_eq!(request, worker_expected);
            write_delivery_ack(&mut stream).unwrap();
        });

        let mut request_factory = || expected.clone();
        notify_existing(&path, &mut request_factory).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn primary_socket_is_private_and_secondary_delivery_requires_listener_ack() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        primary.start_listener(|_| true).unwrap();
        assert!(matches!(
            claim_without_activation_token(path.clone(), uid).unwrap(),
            SingleInstanceStatus::Secondary
        ));
        drop(primary);
        assert!(!path.exists());
    }

    #[test]
    fn second_client_delivers_full_external_launch_to_listener() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        let (tx, rx) = mpsc::sync_channel(1);
        primary
            .start_listener(move |request| tx.send(request).is_ok())
            .unwrap();
        let expected = ExternalLaunchRequest {
            activation_token: None,
            launch: TerminalLaunchSpec {
                working_directory: Some(PathBuf::from("/tmp/secondary")),
                command: vec![OsString::from("program"), OsString::from("arg one")],
                hold: true,
                bridge_launch_id: None,
            },
        };

        assert!(matches!(
            claim_with_request(path, uid, &expected).unwrap(),
            SingleInstanceStatus::Secondary
        ));
        assert_eq!(external_message(rx.recv_timeout(Duration::from_secs(1)).expect("external launch was not delivered")), expected);
    }

    #[test]
    fn listener_preserves_different_sequential_launch_specs_and_order() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        let (tx, rx) = mpsc::sync_channel(2);
        primary
            .start_listener(move |request| tx.send(request).is_ok())
            .unwrap();
        let first = ExternalLaunchRequest {
            activation_token: None,
            launch: TerminalLaunchSpec {
                working_directory: Some(PathBuf::from("/tmp/first")),
                command: vec![OsString::from("first"), OsString::from("arg one")],
                hold: false,
                bridge_launch_id: None,
            },
        };
        let second = ExternalLaunchRequest {
            activation_token: Some("second-token".to_owned()),
            launch: TerminalLaunchSpec {
                working_directory: Some(PathBuf::from("/tmp/second")),
                command: vec![OsString::from("second"), OsString::from("--flag")],
                hold: true,
                bridge_launch_id: None,
            },
        };

        send_request(&path, &first);
        send_request(&path, &second);

        assert_eq!(external_message(rx.recv_timeout(Duration::from_secs(1)).unwrap()), first);
        assert_eq!(external_message(rx.recv_timeout(Duration::from_secs(1)).unwrap()), second);
    }

    #[test]
    fn listener_callback_rejection_does_not_report_secondary_success() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_without_activation_token(path.clone(), uid).unwrap());
        primary.start_listener(|_| false).unwrap();

        let error = claim_without_activation_token(path, uid).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
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
                let mut status = claim_without_activation_token(path, uid);
                let is_primary = matches!(status, Ok(SingleInstanceStatus::Primary(_)));
                if let Ok(SingleInstanceStatus::Primary(primary)) = &mut status {
                    primary.start_listener(|_| true).unwrap();
                }
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
            .write_all(&request_header(
                PROTOCOL_VERSION + 1,
                EXTERNAL_LAUNCH_MESSAGE,
                0,
            ))
            .unwrap();
        drop(unknown);

        let mut unknown_type = UnixStream::connect(&path).unwrap();
        unknown_type
            .write_all(&request_header(PROTOCOL_VERSION, 0x7f, 0))
            .unwrap();
        drop(unknown_type);

        let mut truncated = UnixStream::connect(&path).unwrap();
        truncated
            .write_all(&[PROTOCOL_VERSION, EXTERNAL_LAUNCH_MESSAGE, 0])
            .unwrap();
        drop(truncated);

        let mut oversized_request = UnixStream::connect(&path).unwrap();
        oversized_request
            .write_all(&request_header(
                PROTOCOL_VERSION,
                EXTERNAL_LAUNCH_MESSAGE,
                u32::try_from(MAX_REQUEST_PAYLOAD_BYTES + 1).unwrap(),
            ))
            .unwrap();
        drop(oversized_request);

        send_request(&path, &ExternalLaunchRequest::default());
        let request = rx
            .recv_timeout(Duration::from_secs(1))
            .expect("listener stopped after malformed request");
        assert_eq!(external_message(request), ExternalLaunchRequest::default());
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
