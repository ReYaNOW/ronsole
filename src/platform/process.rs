use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PROCESS_TREE_FORCE_CLEANUP_TIMEOUT: Duration = Duration::from_millis(500);
const PROCESS_TREE_CLEANUP_POLL: Duration = Duration::from_millis(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProcessSnapshot {
    pub process_id: u32,
    pub executable: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub args: Vec<OsString>,
}

pub(crate) fn process_snapshot(process_id: u32) -> Option<ProcessSnapshot> {
    let base = PathBuf::from("/proc").join(process_id.to_string());
    let executable = std::fs::read_link(base.join("exe")).ok();
    let cwd = std::fs::read_link(base.join("cwd")).ok();
    let args = std::fs::read(base.join("cmdline"))
        .ok()
        .map(|bytes| {
            bytes
                .split(|byte| *byte == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| OsString::from_vec(arg.to_vec()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if executable.is_none() && args.is_empty() {
        return None;
    }
    Some(ProcessSnapshot {
        process_id,
        executable,
        cwd,
        args,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ForegroundProcessCandidate {
    process_id: u32,
    depth: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinuxProcessStat {
    process_group: i32,
    session_id: i32,
    state: u8,
}

fn linux_process_stat(process_id: u32) -> Option<LinuxProcessStat> {
    let stat = std::fs::read(
        PathBuf::from("/proc")
            .join(process_id.to_string())
            .join("stat"),
    )
    .ok()?;
    let command_end = stat.iter().rposition(|byte| *byte == b')')?;
    let fields = std::str::from_utf8(stat.get(command_end + 1..)?).ok()?;
    let mut fields = fields.split_ascii_whitespace();
    let state = *fields.next()?.as_bytes().first()?;
    fields.next()?; // ppid
    let process_group = fields.next()?.parse().ok()?;
    let session_id = fields.next()?.parse().ok()?;
    Some(LinuxProcessStat {
        process_group,
        session_id,
        state,
    })
}

fn linux_process_group(process_id: u32) -> Option<u32> {
    linux_process_stat(process_id)?.process_group.try_into().ok()
}

fn linux_process_children(process_id: u32) -> Vec<u32> {
    let children = std::fs::read_to_string(
        PathBuf::from("/proc")
            .join(process_id.to_string())
            .join("task")
            .join(process_id.to_string())
            .join("children"),
    )
    .unwrap_or_default();
    children
        .split_ascii_whitespace()
        .filter_map(|child| child.parse().ok())
        .collect()
}

fn select_effective_foreground_process(
    candidates: &[ForegroundProcessCandidate],
) -> Option<u32> {
    candidates
        .iter()
        .max_by_key(|candidate| (candidate.depth, candidate.process_id))
        .map(|candidate| candidate.process_id)
}

pub(crate) fn foreground_process_snapshot(process_group: u32) -> Option<ProcessSnapshot> {
    let mut stack = Vec::with_capacity(4);
    let mut candidates = Vec::with_capacity(4);
    stack.push(ForegroundProcessCandidate {
        process_id: process_group,
        depth: 0,
    });

    while let Some(candidate) = stack.pop() {
        for child in linux_process_children(candidate.process_id) {
            if linux_process_group(child) == Some(process_group) {
                stack.push(ForegroundProcessCandidate {
                    process_id: child,
                    depth: candidate.depth.saturating_add(1),
                });
            }
        }
        candidates.push(candidate);
    }

    let effective = select_effective_foreground_process(&candidates)?;
    process_snapshot(effective).or_else(|| {
        (effective != process_group)
            .then(|| process_snapshot(process_group))
            .flatten()
    })
}

fn linux_session_process_groups(session_id: i32) -> io::Result<Vec<i32>> {
    let mut groups = Vec::with_capacity(4);
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else {
            continue;
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(process_id) = name.parse::<u32>() else {
            continue;
        };
        let Some(stat) = linux_process_stat(process_id) else {
            continue;
        };
        if stat.session_id == session_id && stat.state != b'Z' && stat.process_group > 0 {
            groups.push(stat.process_group);
        }
    }
    groups.sort_unstable();
    groups.dedup();
    Ok(groups)
}

fn signal_process_groups(process_groups: &[i32], signal: i32) -> io::Result<()> {
    let mut first_error = None;
    for process_group in process_groups {
        if let Err(error) = signal_process_group(*process_group, signal)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

pub struct ProcessTree {
    session_id: i32,
    active: bool,
}

impl ProcessTree {
    pub fn attach_process_id(process_id: u32) -> io::Result<Self> {
        let session_id = i32::try_from(process_id)
            .map_err(|_| io::Error::other("process id does not fit in pid_t"))?;
        let stat = linux_process_stat(process_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "terminal process disappeared before session ownership was established",
            )
        })?;
        if stat.session_id != session_id || stat.process_group != session_id {
            return Err(io::Error::other(
                "terminal child is not the expected portable-pty session leader",
            ));
        }
        Ok(Self {
            session_id,
            active: true,
        })
    }

    fn terminate(&self, force: bool) -> io::Result<()> {
        if !self.active {
            return Ok(());
        }
        let signal = if force { libc::SIGKILL } else { libc::SIGTERM };
        let groups = linux_session_process_groups(self.session_id)?;
        signal_process_groups(&groups, signal)
    }

    pub fn terminate_gracefully(&self) -> io::Result<()> {
        self.terminate(false)
    }

    pub fn terminate_forcefully(&self) -> io::Result<()> {
        self.terminate(true)
    }

    /// Non-blocking check used during graceful shutdown: true once no live
    /// process group remains in the owned session. A `/proc` read failure
    /// reports "still live" so the caller keeps its bounded deadline and then
    /// escalates through `finish_after_owner_exit`.
    pub(crate) fn session_exited(&mut self) -> bool {
        if !self.active {
            return true;
        }
        match linux_session_process_groups(self.session_id) {
            Ok(groups) if groups.is_empty() => {
                self.active = false;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn finish_after_owner_exit(&mut self) {
        if !self.active {
            return;
        }

        let deadline = Instant::now() + PROCESS_TREE_FORCE_CLEANUP_TIMEOUT;
        loop {
            let groups = match linux_session_process_groups(self.session_id) {
                Ok(groups) => groups,
                Err(_) => return,
            };
            if groups.is_empty() {
                self.active = false;
                return;
            }
            let _ = signal_process_groups(&groups, libc::SIGKILL);
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(PROCESS_TREE_CLEANUP_POLL);
        }
    }
}

impl Drop for ProcessTree {
    fn drop(&mut self) {
        self.finish_after_owner_exit();
    }
}

fn signal_process_group(process_group: i32, signal: i32) -> io::Result<()> {
    let result = unsafe { libc::kill(-process_group, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

pub fn resolve_executable(program: &OsStr) -> Option<PathBuf> {
    resolve_executable_with(program, std::env::var_os("PATH").as_deref())
}

fn resolve_executable_with(program: &OsStr, path: Option<&OsStr>) -> Option<PathBuf> {
    let candidate = Path::new(program);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return is_executable_file(candidate).then(|| candidate.to_path_buf());
    }

    let path = path?;
    for directory in std::env::split_paths(path) {
        let direct = directory.join(candidate);
        if is_executable_file(&direct) {
            return Some(direct);
        }
    }
    None
}

/// Returns whether `uri` may be passed to the desktop URI opener.
///
/// Only `http`, `https`, and `file` URIs pass. A `file` URI must name an empty
/// host, `localhost`, or `local_host`, and must have a path below the root.
/// Scheme and host comparisons ignore ASCII case (RFC 3986 sections 3.1 and
/// 3.2.2). Control characters (C0, DEL, C1) are rejected outright, and a
/// non-ASCII host is rejected because IDNA is not applied. Non-ASCII path,
/// query, and fragment are allowed and percent-encoded by `xdg_open_argument`.
pub fn open_uri_allowed(uri: &str, local_host: &str) -> bool {
    if uri.chars().any(char::is_control) {
        return false;
    }
    let Some((scheme, rest)) = uri.split_once(':') else {
        return false;
    };
    let Some(after_slashes) = rest.strip_prefix("//") else {
        return false;
    };
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        let host = after_slashes.split(['/', '?', '#']).next().unwrap_or_default();
        return !after_slashes.is_empty() && host.is_ascii();
    }
    if scheme.eq_ignore_ascii_case("file") {
        let Some(path_start) = after_slashes.find('/') else {
            return false;
        };
        let (host, path) = after_slashes.split_at(path_start);
        if !host.is_ascii() {
            return false;
        }
        // A bare "/" names only the root, so something must follow it.
        if path.len() <= 1 {
            return false;
        }
        return host.is_empty()
            || host.eq_ignore_ascii_case("localhost")
            || host.eq_ignore_ascii_case(local_host);
    }
    false
}

/// Opens `uri` with `xdg-open` on a dedicated thread.
///
/// Callers must check [`open_uri_allowed`] first. File-system checks for
/// `file` URIs run on that thread through [`opener_argument`], so the caller
/// never blocks on disk I/O. The opener runs without a shell, and its thread
/// is handed to the unit-thread reaper so the caller never waits for the child
/// process.
pub fn open_uri(uri: &str) {
    let uri = uri.to_owned();
    match super::spawn_named("ronsole-open-uri", move || {
        if let Some(argument) = opener_argument(&uri) {
            run_xdg_open(&argument);
        }
    }) {
        Ok(worker) => super::reap_unit_thread(worker),
        Err(error) => eprintln!("Ronsole: failed to start URI opener thread: {error}"),
    }
}

/// Returns the argument for `xdg-open`, or `None` when the URI must not be
/// opened. Non-`file` URIs pass through unchanged. A `file` path is resolved
/// through symlinks first, and the decision uses the resolved path. An opened
/// `file` path is passed as its canonical, fully percent-encoded form, never as
/// the original URI, so the opener cannot reinterpret a fragment, query, or
/// `..` from it. A missing path logs once and yields `None`.
fn opener_argument(uri: &str) -> Option<String> {
    let is_file_uri = uri
        .split_once(':')
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("file"));
    if !is_file_uri {
        return Some(xdg_open_argument(uri));
    }
    let path = file_uri_path(uri)?;
    let resolved = match std::fs::canonicalize(&path) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("Ronsole: cannot open link {}: {error}", path.display());
            return None;
        }
    };
    let metadata = match std::fs::metadata(&resolved) {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("Ronsole: cannot open link {}: {error}", resolved.display());
            return None;
        }
    };
    let decision = local_open_decision(
        metadata.is_dir(),
        metadata.is_file(),
        metadata.permissions().mode(),
        &resolved,
    );
    match decision {
        LocalOpenDecision::OpenPath => Some(file_uri_for_path(&resolved)),
        LocalOpenDecision::OpenParent => resolved.parent().map(file_uri_for_path),
    }
}

/// What `xdg-open` receives for a resolved local path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalOpenDecision {
    /// The path itself is opened.
    OpenPath,
    /// The containing directory is opened instead.
    OpenParent,
}

/// Chooses between opening a resolved path and its parent directory. A
/// directory always opens itself. A regular file opens itself unless it has an
/// execute bit (`u`, `g`, or `o`) or a `.desktop` name suffix. Any other kind
/// of entry (device, FIFO, socket) opens its parent.
fn local_open_decision(
    is_directory: bool,
    is_regular_file: bool,
    mode: u32,
    resolved: &Path,
) -> LocalOpenDecision {
    if is_directory {
        return LocalOpenDecision::OpenPath;
    }
    if !is_regular_file {
        return LocalOpenDecision::OpenParent;
    }
    let is_desktop_entry = resolved
        .file_name()
        .is_some_and(|name| has_desktop_suffix(name.as_bytes()));
    if mode & 0o111 != 0 || is_desktop_entry {
        LocalOpenDecision::OpenParent
    } else {
        LocalOpenDecision::OpenPath
    }
}

/// Matches the `.desktop` suffix ignoring ASCII case.
fn has_desktop_suffix(name: &[u8]) -> bool {
    const SUFFIX: &[u8] = b".desktop";
    name.len() >= SUFFIX.len() && name[name.len() - SUFFIX.len()..].eq_ignore_ascii_case(SUFFIX)
}

/// Extracts the local path of a `file` URI, skipping its host. Host checks are
/// the job of [`open_uri_allowed`]. Percent escapes decode to raw path bytes.
fn file_uri_path(uri: &str) -> Option<PathBuf> {
    let (_, rest) = uri.split_once(':')?;
    let after_slashes = rest.strip_prefix("//")?;
    let path_start = after_slashes.find('/')?;
    let bytes = percent_decode(&after_slashes[path_start..]);
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// Decodes `%XX` escapes into raw bytes. A malformed escape is copied as-is.
fn percent_decode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            if let Some(value) = bytes.get(index + 1..index + 3).and_then(hex_pair_value) {
                decoded.push(value);
                index += 3;
                continue;
            }
        }
        decoded.push(byte);
        index += 1;
    }
    decoded
}

fn hex_pair_value(pair: &[u8]) -> Option<u8> {
    match pair {
        [high, low] => Some((hex_digit_value(*high)? << 4) | hex_digit_value(*low)?),
        _ => None,
    }
}

fn hex_digit_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

/// Builds a `file://` URI for an absolute path. Bytes outside the RFC 3986
/// unreserved set and `/` are percent-encoded.
fn file_uri_for_path(path: &Path) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            uri.push(char::from(byte));
        } else {
            uri.push('%');
            uri.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
            uri.push(char::from(HEX_DIGITS[usize::from(byte & 0x0F)]));
        }
    }
    uri
}

/// Returns the argument handed to `xdg-open` for a URI accepted by
/// [`open_uri_allowed`]. Non-ASCII bytes are percent-encoded, so a UTF-8 path
/// reaches the opener as a valid URI.
fn xdg_open_argument(uri: &str) -> String {
    percent_encode_non_ascii(&file_uri_argument(uri))
}

/// Percent-encodes every non-ASCII character of `text` as uppercase `%XX` per
/// UTF-8 byte. ASCII characters are copied unchanged.
fn percent_encode_non_ascii(text: &str) -> String {
    const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(text.len());
    let mut buffer = [0u8; 4];
    for ch in text.chars() {
        if ch.is_ascii() {
            encoded.push(ch);
            continue;
        }
        for byte in ch.encode_utf8(&mut buffer).bytes() {
            encoded.push('%');
            encoded.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX_DIGITS[usize::from(byte & 0x0F)]));
        }
    }
    encoded
}

/// Rewrites a `file` URI to the empty-host form `file:///path`, because
/// `xdg-open` strips `localhost` and the local host name case-sensitively and
/// would otherwise treat `file://MYHOST/x` as a relative path. Other URIs are
/// returned unchanged.
fn file_uri_argument(uri: &str) -> String {
    let Some((scheme, rest)) = uri.split_once(':') else {
        return uri.to_owned();
    };
    if !scheme.eq_ignore_ascii_case("file") {
        return uri.to_owned();
    }
    let Some(after_slashes) = rest.strip_prefix("//") else {
        return uri.to_owned();
    };
    let path = after_slashes
        .find('/')
        .map_or("", |index| &after_slashes[index..]);
    format!("file://{path}")
}

fn run_xdg_open(argument: &str) {
    let Some(opener) = resolve_executable(OsStr::new("xdg-open")) else {
        eprintln!("Ronsole: cannot open link, xdg-open was not found");
        return;
    };
    // The opener must not share the terminal's stdio.
    let spawned = Command::new(opener)
        .arg(argument)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            eprintln!("Ronsole: failed to run xdg-open: {error}");
            return;
        }
    };
    if let Err(error) = child.wait() {
        eprintln!("Ronsole: failed to wait for xdg-open: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(process_id: u32, depth: usize) -> ForegroundProcessCandidate {
        ForegroundProcessCandidate { process_id, depth }
    }

    fn wait_for_process_exit(process_id: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if linux_process_stat(process_id).is_none() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        linux_process_stat(process_id).is_none()
    }

    struct PtySessionCleanupGuard {
        child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
        tree: Option<ProcessTree>,
        job_pid_path: Option<PathBuf>,
    }

    impl Drop for PtySessionCleanupGuard {
        fn drop(&mut self) {
            if let Some(tree) = self.tree.as_mut() {
                let _ = tree.terminate_forcefully();
            }
            if let Some(child) = self.child.as_mut() {
                let _ = child.kill();
                let _ = child.wait();
            }
            if let Some(tree) = self.tree.as_mut() {
                tree.finish_after_owner_exit();
            }
            if let Some(path) = self.job_pid_path.as_ref() {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    #[test]
    fn foreground_group_leader_is_effective_without_children() {
        assert_eq!(
            select_effective_foreground_process(&[candidate(100, 0)]),
            Some(100)
        );
    }

    #[test]
    fn foreground_wrapper_prefers_deepest_child_in_same_group() {
        assert_eq!(
            select_effective_foreground_process(&[
                candidate(100, 0),
                candidate(101, 1),
                candidate(102, 2),
            ]),
            Some(102)
        );
    }

    #[test]
    fn executable_resolution_requires_unix_execute_permission() {
        use std::time::{SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ronsole-resolve-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let executable = root.join("probe-shell");
        std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let search_path = std::env::join_paths([&root]).unwrap();

        assert_eq!(
            resolve_executable_with(OsStr::new("probe-shell"), Some(search_path.as_os_str())),
            Some(executable.clone())
        );

        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o644))
            .unwrap();
        assert_eq!(
            resolve_executable_with(OsStr::new("probe-shell"), Some(search_path.as_os_str())),
            None
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn process_tree_force_kills_every_member_of_the_owned_group() {
        use std::os::unix::process::CommandExt;

        let mut leader_command = std::process::Command::new("/bin/sleep");
        leader_command.arg("3").process_group(0);
        let mut leader = leader_command.spawn().unwrap();
        let process_group = leader.id();

        let mut member_command = std::process::Command::new("/bin/sleep");
        member_command
            .arg("3")
            .process_group(i32::try_from(process_group).unwrap());
        let mut member = member_command.spawn().unwrap();

        assert_eq!(linux_process_group(leader.id()), Some(process_group));
        assert_eq!(linux_process_group(member.id()), Some(process_group));

        signal_process_group(i32::try_from(process_group).unwrap(), libc::SIGKILL).unwrap();
        let leader_status = leader.wait().unwrap();
        let member_status = member.wait().unwrap();

        assert!(!leader_status.success());
        assert!(!member_status.success());
    }

    #[test]
    fn real_pty_shutdown_kills_background_job_in_separate_process_group() {
        use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
        use std::io::Write;

        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("/bin/bash");
        command.arg("--noprofile");
        command.arg("--norc");
        command.arg("-i");
        command.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);

        let shell_pid = child.process_id().unwrap();
        let tree = ProcessTree::attach_process_id(shell_pid).unwrap();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let job_pid_path = std::env::temp_dir().join(format!(
            "ronsole-pty-job-{}-{unique}.pid",
            std::process::id()
        ));
        let mut guard = PtySessionCleanupGuard {
            child: Some(child),
            tree: Some(tree),
            job_pid_path: Some(job_pid_path.clone()),
        };
        let shell_stat = linux_process_stat(shell_pid).unwrap();
        assert_eq!(shell_stat.process_group, i32::try_from(shell_pid).unwrap());
        assert_eq!(shell_stat.session_id, i32::try_from(shell_pid).unwrap());

        let mut writer = pair.master.take_writer().unwrap();
        let command = format!(
            "set -m; sleep 30 & printf '%s\n' \"$!\" > {}\r",
            job_pid_path.display()
        );
        writer.write_all(command.as_bytes()).unwrap();
        writer.flush().unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let job_pid = loop {
            if let Ok(pid) = std::fs::read_to_string(&job_pid_path)
                && let Ok(pid) = pid.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "bash did not report the background job pid");
            std::thread::sleep(Duration::from_millis(10));
        };
        let job_stat = linux_process_stat(job_pid)
            .expect("background job disappeared before inspection");

        assert_ne!(shell_stat.process_group, job_stat.process_group);
        assert_eq!(shell_stat.session_id, job_stat.session_id);
        println!(
            "real PTY session: shell_pid={shell_pid} shell_pgid={} job_pid={job_pid} job_pgid={} sid={}",
            shell_stat.process_group, job_stat.process_group, shell_stat.session_id
        );

        guard.tree.as_ref().unwrap().terminate_forcefully().unwrap();
        let shell_status = guard.child.as_mut().unwrap().wait().unwrap();
        guard.tree.as_mut().unwrap().finish_after_owner_exit();

        assert!(!shell_status.success());
        assert!(
            wait_for_process_exit(shell_pid, Duration::from_secs(2)),
            "terminal shell remained after session shutdown"
        );
        assert!(
            wait_for_process_exit(job_pid, Duration::from_secs(2)),
            "background job in separate PGID remained after session shutdown"
        );
        println!("real PTY cleanup: shell_gone=true job_gone=true");
    }

    #[test]
    fn open_uri_allowed_filters_schemes() {
        // Expected values come from the Task 11 brief, not from the implementation.
        let local_host = "myhost";
        let allowed = [
            "https://a",
            "http://a",
            "file:///x",
            "file://localhost/x",
            "file://myhost/x",
            "file://MYHOST/x",
            "HTTPS://A",
        ];
        for uri in allowed {
            assert!(open_uri_allowed(uri, local_host), "expected allowed: {uri:?}");
        }
        let denied = [
            "file://other/x",
            "javascript:x",
            "mailto:a@b",
            "ftp://x",
            "",
            "file:///",
            "file://localhost",
            "https:",
            "https:/a",
            "file:x",
            "/etc/passwd",
            "https://a\nb",
            "https://a\tb",
            "https://a\x1b[31m",
            "https://a\x7f",
            "https://a/\u{0}",
            "https://ß",
        ];
        for uri in denied {
            assert!(!open_uri_allowed(uri, local_host), "expected denied: {uri:?}");
        }
    }

    #[test]
    fn xdg_open_argument_drops_file_host() {
        // Expected values come from the Task 11 brief, not from the implementation.
        assert_eq!(xdg_open_argument("file://MYHOST/x"), "file:///x");
        assert_eq!(xdg_open_argument("file://localhost/x"), "file:///x");
        assert_eq!(xdg_open_argument("file:///x"), "file:///x");
        assert_eq!(
            xdg_open_argument("https://example.com/x"),
            "https://example.com/x"
        );
    }

    #[test]
    fn open_uri_percent_encodes_non_ascii_path() {
        // Expected values come from the G2-5 ruling, not from the implementation.
        assert!(open_uri_allowed("https://example.org/путь", "myhost"));
        assert_eq!(
            xdg_open_argument("https://example.org/путь"),
            "https://example.org/%D0%BF%D1%83%D1%82%D1%8C"
        );
        assert_eq!(
            xdg_open_argument("https://example.org/?q=путь#фрагмент"),
            "https://example.org/?q=%D0%BF%D1%83%D1%82%D1%8C#%D1%84%D1%80%D0%B0%D0%B3%D0%BC%D0%B5%D0%BD%D1%82"
        );
        // Already encoded input is not encoded twice.
        assert_eq!(xdg_open_argument("https://a.org/%D0%BF"), "https://a.org/%D0%BF");
    }

    #[test]
    fn open_uri_refuses_non_ascii_host_and_c1_control() {
        // Expected values come from the G2-5 ruling: no IDNA, and C1 controls are refused.
        assert!(!open_uri_allowed("https://пример.рф/", "myhost"));
        assert!(!open_uri_allowed("https://a.org/\u{85}", "myhost"));
    }

    /// Canonical temporary directory for one open test, removed when dropped.
    struct OpenTestDir(PathBuf);

    impl std::ops::Deref for OpenTestDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for OpenTestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn open_test_dir(name: &str) -> OpenTestDir {
        let dir = std::env::temp_dir().join(format!("ronsole-open-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create open test dir");
        OpenTestDir(std::fs::canonicalize(&dir).expect("canonicalize open test dir"))
    }

    fn make_open_test_file(path: &Path, mode: u32) {
        std::fs::write(path, b"x").expect("write open test file");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("chmod open test file");
    }

    #[test]
    fn opener_argument_opens_plain_file_and_directory_themselves() {
        // Expected values come from spec section 6.3 (Q1 O2), not from the implementation.
        let dir = open_test_dir("plain");
        let file = dir.join("a.txt");
        make_open_test_file(&file, 0o644);
        let file_uri = file_uri_for_path(&file);
        assert_eq!(
            opener_argument(&file_uri),
            Some(format!("file://{}/a.txt", dir.display()))
        );
        let dir_uri = file_uri_for_path(&dir);
        assert_eq!(opener_argument(&dir_uri), Some(dir_uri.clone()));
    }

    #[test]
    fn opener_argument_opens_parent_of_executable() {
        // Expected values come from spec section 6.3 (Q1 O2), not from the implementation.
        let dir = open_test_dir("exec");
        make_open_test_file(&dir.join("tool"), 0o755);
        make_open_test_file(&dir.join("other.bin"), 0o001);
        make_open_test_file(&dir.join("group.bin"), 0o010);
        let parent = Some(file_uri_for_path(&dir));
        assert_eq!(opener_argument(&file_uri_for_path(&dir.join("tool"))), parent);
        assert_eq!(opener_argument(&file_uri_for_path(&dir.join("other.bin"))), parent);
        assert_eq!(opener_argument(&file_uri_for_path(&dir.join("group.bin"))), parent);
    }

    #[test]
    fn opener_argument_opens_parent_of_desktop_entry_any_case() {
        // Expected values come from spec section 6.3 (Q1 O2), not from the implementation.
        let dir = open_test_dir("desktop");
        let parent = Some(file_uri_for_path(&dir));
        for name in ["x.desktop", "X.DESKTOP"] {
            let entry = dir.join(name);
            make_open_test_file(&entry, 0o644);
            assert_eq!(opener_argument(&file_uri_for_path(&entry)), parent, "{name}");
        }
    }

    #[test]
    fn opener_argument_resolves_symlink_before_deciding() {
        // Expected values come from spec section 6.3 (Q1 O2), not from the implementation.
        let dir = open_test_dir("symlink");
        let bin_dir = dir.join("bin");
        std::fs::create_dir_all(&bin_dir).expect("create bin test dir");
        make_open_test_file(&bin_dir.join("tool"), 0o755);
        let link = dir.join("a.txt");
        std::os::unix::fs::symlink(bin_dir.join("tool"), &link).expect("create test symlink");
        assert_eq!(
            opener_argument(&file_uri_for_path(&link)),
            Some(file_uri_for_path(&bin_dir))
        );
    }

    #[test]
    fn opener_argument_decodes_percent_escapes_in_path() {
        // Expected values come from spec section 6.3 (Q1 O2), not from the implementation.
        let dir = open_test_dir("escape");
        make_open_test_file(&dir.join("a b.txt"), 0o644);
        let uri = format!("file://{}/a%20b.txt", dir.display());
        assert_eq!(
            opener_argument(&uri),
            Some(format!("file://{}/a%20b.txt", dir.display()))
        );
    }

    #[test]
    fn opener_argument_resolves_symlink_to_desktop_entry() {
        // Expected values come from spec section 6.3 (Q1 O2), not from the implementation.
        let dir = open_test_dir("symlink_desktop");
        make_open_test_file(&dir.join("entry.desktop"), 0o644);
        std::os::unix::fs::symlink(dir.join("entry.desktop"), dir.join("link.txt"))
            .expect("create test symlink");
        assert_eq!(
            opener_argument(&file_uri_for_path(&dir.join("link.txt"))),
            Some(file_uri_for_path(&dir))
        );
    }

    #[test]
    fn opener_argument_does_not_pass_raw_fragment_to_opener() {
        // Expected values come from spec section 6.3 (Q1 O2) and the O2-1 review:
        // xdg-open strips a literal "#...", so the passed path must be encoded.
        let dir = open_test_dir("fragment");
        make_open_test_file(&dir.join("tool"), 0o755);
        make_open_test_file(&dir.join("tool#x"), 0o644);
        let uri = format!("file://{}/tool#x", dir.display());
        assert_eq!(
            opener_argument(&uri),
            Some(format!("file://{}/tool%23x", dir.display()))
        );
    }

    #[test]
    fn opener_argument_passes_canonical_path_not_dot_dot_uri() {
        // Expected values come from spec section 6.3 (Q1 O2) and the O2-1 review:
        // the opener receives the canonical path, so "..": through a symlink is gone.
        let dir = open_test_dir("dotdot");
        std::fs::create_dir_all(dir.join("sub")).expect("create sub test dir");
        std::os::unix::fs::symlink(dir.join("sub"), dir.join("link")).expect("create test symlink");
        make_open_test_file(&dir.join("x.txt"), 0o644);
        let uri = format!("file://{}/link/../x.txt", dir.display());
        assert_eq!(
            opener_argument(&uri),
            Some(format!("file://{}/x.txt", dir.display()))
        );
    }

    #[test]
    fn opener_argument_encodes_malformed_percent_sequence_in_passed_path() {
        // Expected values come from spec section 6.3 (Q1 O2) and the O2-1 review:
        // xdg-open runs its path through printf, so a raw "%" must reach it encoded.
        let dir = open_test_dir("percent");
        make_open_test_file(&dir.join("evil.desk%stop"), 0o644);
        let uri = format!("file://{}/evil.desk%stop", dir.display());
        assert_eq!(
            opener_argument(&uri),
            Some(format!("file://{}/evil.desk%25stop", dir.display()))
        );
    }

    #[test]
    fn local_open_decision_opens_parent_of_non_regular_entry() {
        // Expected values come from spec section 6.3 (Q1 O2), not from the implementation.
        let path = Path::new("/x/entry");
        assert_eq!(
            local_open_decision(false, true, 0o644, path),
            LocalOpenDecision::OpenPath
        );
        // Character device such as /dev/zero: mode 0o666 has no execute bit.
        assert_eq!(
            local_open_decision(false, false, 0o666, path),
            LocalOpenDecision::OpenParent
        );
        // FIFO or socket with no execute bit.
        assert_eq!(
            local_open_decision(false, false, 0o644, path),
            LocalOpenDecision::OpenParent
        );
        assert_eq!(
            local_open_decision(true, false, 0o755, path),
            LocalOpenDecision::OpenPath
        );
    }

    #[test]
    fn opener_argument_opens_parent_of_device() {
        assert_eq!(
            opener_argument("file:///dev/null"),
            Some("file:///dev".to_string())
        );
    }

    #[test]
    fn opener_argument_drops_missing_path() {
        // Expected values come from spec section 6.3 (Q1 O2), not from the implementation.
        let dir = open_test_dir("missing");
        assert_eq!(opener_argument(&file_uri_for_path(&dir.join("absent.txt"))), None);
    }
}
