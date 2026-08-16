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
const EXTERNAL_LAUNCH_COMMAND: u8 = 1;
const CLAIM_RETRIES: usize = 8;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_millis(250);

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
        F: FnMut() -> bool + Send + 'static,
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
                let mut command = [0_u8; 1];
                if stream.read_exact(&mut command).is_ok()
                    && command[0] == EXTERNAL_LAUNCH_COMMAND
                    && !external_launch()
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
    let _startup_lock = StartupLock::acquire(&startup_lock_path(&socket_path))?;
    for _ in 0..CLAIM_RETRIES {
        match notify_existing(&socket_path) {
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

fn notify_existing(socket_path: &Path) -> io::Result<()> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT))?;
    stream.write_all(&[EXTERNAL_LAUNCH_COMMAND])
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
    fn first_owner_is_primary_second_launch_is_secondary_and_socket_is_private() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let primary = take_primary(claim_at(path.clone(), uid).unwrap());
        let metadata = fs::symlink_metadata(&path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert!(matches!(
            claim_at(path.clone(), uid).unwrap(),
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
        let mut primary = take_primary(claim_at(path.clone(), uid).unwrap());
        let (tx, rx) = mpsc::sync_channel(1);
        primary.start_listener(move || tx.send(()).is_ok()).unwrap();

        assert!(matches!(
            claim_at(path, uid).unwrap(),
            SingleInstanceStatus::Secondary
        ));
        rx.recv_timeout(Duration::from_secs(1))
            .expect("external launch was not delivered");
    }

    #[test]
    fn stale_socket_is_removed_and_reclaimed() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let listener = UnixListener::bind(&path).unwrap();
        drop(listener);
        assert!(path.exists());

        let primary = take_primary(claim_at(path.clone(), effective_uid()).unwrap());
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
                let status = claim_at(path, uid).unwrap();
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
    fn unknown_and_truncated_commands_do_not_stop_listener() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_at(path.clone(), uid).unwrap());
        let (tx, rx) = mpsc::sync_channel(1);
        primary.start_listener(move || tx.send(()).is_ok()).unwrap();

        let mut unknown = UnixStream::connect(&path).unwrap();
        unknown.write_all(&[0x7f]).unwrap();
        drop(unknown);
        drop(UnixStream::connect(&path).unwrap());

        assert!(matches!(
            claim_at(path, uid).unwrap(),
            SingleInstanceStatus::Secondary
        ));
        rx.recv_timeout(Duration::from_secs(1))
            .expect("listener stopped after malformed command");
    }

    #[test]
    fn existing_non_socket_is_never_deleted_as_stale() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        fs::write(&path, b"not a socket").unwrap();
        let error = claim_at(path.clone(), effective_uid()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"not a socket");
    }

    #[test]
    fn dropping_primary_stops_listener_and_allows_clean_reclaim() {
        let dir = TestDir::new();
        let path = dir.socket_path();
        let uid = effective_uid();
        let mut primary = take_primary(claim_at(path.clone(), uid).unwrap());
        primary.start_listener(|| true).unwrap();
        drop(primary);
        assert!(!path.exists());

        let replacement = take_primary(claim_at(path, uid).unwrap());
        drop(replacement);
    }
}
