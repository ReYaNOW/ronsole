use crate::launch::TerminalLaunchSpec;
use crate::platform::{self, ProcessTree};
use ronsole_term::{AdvanceOutcome, TERMINAL_TITLE_MAX_BYTES, Term, TermSink};
use crate::wake::WakeHandle;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const INITIAL_COLS: u16 = 200;
const INITIAL_ROWS: u16 = 60;
/// Time every process group of a closing tab session gets after SIGTERM before
/// the remaining groups are SIGKILLed. Long enough for TUI programs (Claude Code
/// CLI) to restore terminal state and clear their crash markers.
const TERMINAL_SHUTDOWN_GRACE: Duration = Duration::from_secs(3);
const TERMINAL_SHUTDOWN_POLL: Duration = Duration::from_millis(10);
const TERMINAL_TITLE_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
pub(crate) const TERMINAL_OUTPUT_BATCH_IDLE: Duration = Duration::from_millis(8);
const TERMINAL_OUTPUT_BATCH_MAX: Duration = Duration::from_millis(32);
const TERMINAL_OUTPUT_BATCH_MAX_CHUNKS: usize = 32;
const TERMINAL_OUTPUT_QUEUE_CAPACITY: usize = 16;
pub(crate) const TERMINAL_REPLY_QUEUE_CAPACITY: usize = 32;
pub(crate) const TERMINAL_CLEANUP_QUEUE_CAPACITY: usize = 16;
const TERMINAL_READ_BUFFER_SIZE: usize = 65_536;
const TERMINAL_EXIT_BEFORE_OUTPUT: &[u8] = b"Ronsole terminal exited before producing output\r\n";

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProgrammedTerminalTitle {
    text: String,
    generation: u64,
    observation_serial: u64,
}

#[derive(Debug)]
pub(crate) struct TerminalTitleState {
    fallback: String,
    detected: Option<String>,
    programmed: Option<ProgrammedTerminalTitle>,
    display_suffix: Box<str>,
    generation: u64,
    observation_serial: u64,
    /// Whether the tab's foreground process group leader is a `claude` process.
    claude_foreground: bool,
}

pub(crate) type TerminalTitleCache = Arc<Mutex<TerminalTitleState>>;

pub(crate) const TERMINAL_TAB_TOKEN_ENV: &str = "RONSOLE_TAB_TOKEN";
const TAB_TOKEN_BYTES: usize = 16;

/// Per-tab secret exported to the child as `TERMINAL_TAB_TOKEN_ENV`.
/// Empty when the OS refused randomness: the tab then has no input channel.
pub(crate) struct TabToken(String);

impl TabToken {
    pub(crate) fn generate() -> Self {
        let mut bytes = [0u8; TAB_TOKEN_BYTES];
        if fill_os_random(&mut bytes).is_err() {
            return Self(String::new());
        }
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut token = String::with_capacity(TAB_TOKEN_BYTES * 2);
        for byte in bytes {
            token.push(char::from(HEX[usize::from(byte >> 4)]));
            token.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(token)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for TabToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_tuple("TabToken").field(&"<redacted>").finish()
    }
}

fn fill_os_random(buffer: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        let rest = &mut buffer[filled..];
        // SAFETY: `rest` is a valid writable buffer of `rest.len()` bytes.
        let written = unsafe { libc::getrandom(rest.as_mut_ptr().cast(), rest.len(), 0) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::other("getrandom returned no bytes"));
        }
        filled += written.unsigned_abs();
    }
    Ok(())
}

impl TerminalTitleState {
    pub(crate) fn new(fallback: String) -> Self {
        Self {
            fallback,
            detected: None,
            programmed: None,
            display_suffix: Box::from(""),
            generation: 0,
            observation_serial: 0,
            claude_foreground: false,
        }
    }

    pub(crate) fn new_numbered(fallback: String, display_number: u64) -> Self {
        let mut state = Self::new(fallback);
        state.display_suffix = format!(" ({display_number})").into_boxed_str();
        state
    }

    pub(crate) fn set_fallback(&mut self, fallback: String) {
        self.fallback = fallback;
    }

    pub(crate) fn set_programmed(&mut self, text: String) {
        self.programmed = Some(ProgrammedTerminalTitle {
            text,
            generation: self.generation,
            observation_serial: self.observation_serial,
        });
    }

    pub(crate) fn clear_programmed(&mut self) {
        self.programmed = None;
    }

    pub(crate) fn claude_foreground(&self) -> bool {
        self.claude_foreground
    }

    /// Stores the flag and returns whether it changed.
    pub(crate) fn set_claude_foreground(&mut self, value: bool) -> bool {
        let changed = self.claude_foreground != value;
        self.claude_foreground = value;
        changed
    }

    fn observe_unchanged(&mut self) {
        self.observation_serial = self.observation_serial.wrapping_add(1);
    }

    fn observe_detected(&mut self, detected: String) {
        self.observation_serial = self.observation_serial.wrapping_add(1);
        self.detected = Some(detected);
    }

    fn observe_transition(&mut self, detected: Option<String>, carry_recent_programmed: bool) {
        let previous_serial = self.observation_serial;
        let previous_generation = self.generation;
        self.observation_serial = self.observation_serial.wrapping_add(1);
        self.generation = self.generation.wrapping_add(1);
        if carry_recent_programmed
            && let Some(programmed) = self.programmed.as_mut()
            && programmed.generation == previous_generation
            && programmed.observation_serial == previous_serial
        {
            programmed.generation = self.generation;
        }
        self.detected = detected;
    }

    fn resolved(&self) -> &str {
        if let Some(detected) = self.detected.as_deref() {
            detected
        } else if let Some(programmed) = self
            .programmed
            .as_ref()
            .filter(|programmed| programmed.generation == self.generation)
        {
            &programmed.text
        } else {
            &self.fallback
        }
    }

    pub(crate) fn write_resolved(&self, output: &mut String) {
        output.clear();
        output.push_str(self.resolved());
        output.push_str(&self.display_suffix);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TerminalShellSpec {
    pub executable: PathBuf,
    pub title: String,
}

struct OutputChunk {
    bytes: Vec<u8>,
    len: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TerminalOutputBatchAction {
    Collect,
    FlushAndContinue,
    FlushAndRedraw,
}

pub(crate) struct TerminalProcess {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master_pty: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    /// Shell pid, which is also its session and initial foreground process group.
    shell_pid: u32,
    tree: ProcessTree,
    title_stop_tx: Option<std::sync::mpsc::Sender<()>>,
    title_worker: Option<JoinHandle<()>>,
    /// PTY writer thread; it can block inside a write, so teardown reaps it
    /// through `platform::reap_unit_thread` instead of joining on the caller.
    writer_worker: Option<JoinHandle<()>>,
    finished: bool,
    shutdown_complete: bool,
}

struct TerminalCleanupJob {
    process: TerminalProcess,
    wake: Option<WakeHandle>,
}

pub(crate) struct TerminalCleanupWorker {
    sender: Option<SyncSender<TerminalCleanupJob>>,
    worker: Option<JoinHandle<()>>,
}

impl TerminalCleanupWorker {
    pub(crate) fn new() -> Self {
        let (sender, receiver) =
            std::sync::mpsc::sync_channel::<TerminalCleanupJob>(TERMINAL_CLEANUP_QUEUE_CAPACITY);
        let worker = std::thread::Builder::new()
            .name("ronsole-terminal-cleanup".to_string())
            .spawn(move || run_terminal_cleanup_worker(&receiver))
            .ok();
        Self {
            sender: worker.as_ref().map(|_| sender),
            worker,
        }
    }

    pub(crate) fn try_enqueue(
        &self,
        process: TerminalProcess,
        wake: Option<WakeHandle>,
    ) -> Result<(), TerminalProcess> {
        let Some(sender) = self.sender.as_ref() else {
            return Err(process);
        };
        match sender.try_send(TerminalCleanupJob { process, wake }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => Err(job.process),
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        self.sender.is_some()
    }

    pub(crate) fn shutdown_and_join(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// Closed tabs shut down concurrently off the UI thread: each job gets SIGTERM
/// on arrival and its own `TERMINAL_SHUTDOWN_GRACE` deadline, so one stubborn
/// session never delays the next. After the sender is dropped the worker still
/// drains queued jobs and escalates every active one before returning, which
/// makes `TerminalCleanupWorker::shutdown_and_join` a bounded kill barrier.
fn run_terminal_cleanup_worker(receiver: &Receiver<TerminalCleanupJob>) {
    let mut active: Vec<(TerminalCleanupJob, Instant)> = Vec::new();
    let mut connected = true;
    loop {
        if active.is_empty() {
            if !connected {
                return;
            }
            match receiver.recv() {
                Ok(job) => active.push(start_terminal_cleanup_job(job)),
                Err(_) => return,
            }
        }
        while connected {
            match receiver.try_recv() {
                Ok(job) => active.push(start_terminal_cleanup_job(job)),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => connected = false,
            }
        }

        let now = Instant::now();
        active.retain_mut(|(job, deadline)| {
            if !job.process.poll_shutdown() && now < *deadline {
                return true;
            }
            job.process.force_shutdown();
            job.process.finish_shutdown();
            if let Some(wake) = job.wake.take() {
                wake.wake();
            }
            false
        });
        if !active.is_empty() {
            std::thread::sleep(TERMINAL_SHUTDOWN_POLL);
        }
    }
}

fn start_terminal_cleanup_job(mut job: TerminalCleanupJob) -> (TerminalCleanupJob, Instant) {
    job.process.begin_shutdown();
    (job, Instant::now() + TERMINAL_SHUTDOWN_GRACE)
}

impl Drop for TerminalCleanupWorker {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

impl TerminalProcess {
    pub(crate) fn spawn(
        grid: Arc<Mutex<Term>>,
        bridge_screen_lock: Arc<Mutex<()>>,
        title_cache: TerminalTitleCache,
        wake: Option<WakeHandle>,
        host: HostChannels,
        launch: TerminalLaunchSpec,
        tab_token: &TabToken,
    ) -> io::Result<Self> {
        let (command, cwd, process_title, spawn_target) =
            terminal_launch_command(launch, tab_token)?;
        let fallback = terminal_fallback_title(Some(&cwd), &process_title);
        platform::lock_recover(&title_cache).set_fallback(fallback);

        let pty_system = NativePtySystem::default();
        let pair = pty_system
            .openpty(PtySize {
                rows: INITIAL_ROWS,
                cols: INITIAL_COLS,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| io::Error::other(format!("failed to open PTY: {error}")))?;

        let mut child = pair.slave.spawn_command(command).map_err(|error| {
            io::Error::other(format!("failed to spawn {spawn_target}: {error}"))
        })?;
        drop(pair.slave);

        let process_id = child.process_id().ok_or_else(|| {
            io::Error::other("terminal backend did not expose the child process id")
        })?;
        let mut tree = match ProcessTree::attach_process_id(process_id) {
            Ok(tree) => tree,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    error.kind(),
                    format!("failed to own terminal process tree: {error}"),
                ));
            }
        };

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| io::Error::other(format!("failed to clone PTY reader: {error}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| io::Error::other(format!("failed to take PTY writer: {error}")))?;
        let writer = Arc::new(Mutex::new(writer));
        let master_pty = Arc::new(Mutex::new(pair.master));

        let (title_stop_tx, title_worker) = match install_terminal_title_refresh(
            master_pty.clone(),
            title_cache.clone(),
            process_title,
            cwd,
            wake.clone(),
        ) {
            Ok(handles) => handles,
            Err(error) => {
                let _ = tree.terminate_forcefully();
                let _ = child.kill();
                let _ = child.wait();
                tree.finish_after_owner_exit();
                return Err(error);
            }
        };

        let writer_worker = match install_terminal_io_threads(
            &grid,
            bridge_screen_lock,
            reader,
            writer.clone(),
            wake,
            title_cache,
            host,
        ) {
            Ok(writer_worker) => writer_worker,
            Err(error) => {
                let _ = title_stop_tx.send(());
                platform::reap_unit_thread(title_worker);
                let _ = tree.terminate_forcefully();
                let _ = child.kill();
                let _ = child.wait();
                tree.finish_after_owner_exit();
                return Err(error);
            }
        };

        Ok(Self {
            writer,
            master_pty,
            child,
            shell_pid: process_id,
            tree,
            title_stop_tx: Some(title_stop_tx),
            title_worker: Some(title_worker),
            writer_worker: Some(writer_worker),
            finished: false,
            shutdown_complete: false,
        })
    }

    pub(crate) fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| io::Error::other("terminal writer lock is poisoned"))?;
        writer.write_all(bytes)?;
        writer.flush()
    }

    /// Replaces the PTY writer with one that always fails, to exercise write errors.
    #[cfg(test)]
    pub(crate) fn break_writer_for_test(&self) {
        struct BrokenWriter;
        impl Write for BrokenWriter {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        *platform::lock_recover(&self.writer) = Box::new(BrokenWriter);
    }

    /// Foreground process group leader of the PTY (`tcgetpgrp` on the master).
    pub(crate) fn process_group_leader(&self) -> Option<u32> {
        platform::lock_recover(&self.master_pty)
            .process_group_leader()
            .and_then(|pid| u32::try_from(pid).ok())
    }

    /// Name of the foreground job while it is running instead of the shell. Background
    /// jobs (`&`) leave the shell as the foreground group, so they never report a name.
    /// Walks `/proc`, so call it only at the moment a close is requested.
    pub(crate) fn busy_foreground_name(&self) -> Option<String> {
        busy_job_label(self.process_group_leader(), self.shell_pid, |process_group| {
            platform::foreground_process_snapshot(process_group)
                .map(|snapshot| terminal_process_program_name(&snapshot, ""))
                .filter(|name| !name.is_empty())
        })
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        let master = self
            .master_pty
            .lock()
            .map_err(|_| io::Error::other("terminal PTY lock is poisoned"))?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| io::Error::other(format!("failed to resize PTY: {error}")))
    }

    pub(crate) fn try_wait(&mut self) -> io::Result<bool> {
        if self.finished {
            return Ok(true);
        }
        if self.child.try_wait()?.is_some() {
            self.finished = true;
            self.stop_title_refresh();
        }
        Ok(self.finished)
    }

    fn stop_title_refresh(&mut self) {
        if let Some(stop_tx) = self.title_stop_tx.take() {
            let _ = stop_tx.send(());
        }
        if let Some(worker) = self.title_worker.take() {
            platform::reap_unit_thread(worker);
        }
    }

    /// Blocking session shutdown: SIGTERM to every session group, up to
    /// `TERMINAL_SHUTDOWN_GRACE` for all of them to exit, then SIGKILL.
    pub(crate) fn shutdown(&mut self) {
        shutdown_terminal_processes(std::slice::from_mut(self));
    }

    /// Phase 1: SIGTERM every live process group of the owned session, not
    /// only the shell, so foreground TUI jobs can exit cleanly.
    fn begin_shutdown(&mut self) {
        if self.shutdown_complete {
            return;
        }
        self.stop_title_refresh();
        let _ = self.tree.terminate_gracefully();
    }

    /// Phase 2, non-blocking: reaps the shell and reports whether the whole
    /// session is gone. The shell exiting first does not end the wait; the
    /// `/proc` session scan runs only after the shell is reaped.
    fn poll_shutdown(&mut self) -> bool {
        if self.shutdown_complete {
            return true;
        }
        if !self.finished {
            match self.child.try_wait() {
                Ok(Some(_)) => self.finished = true,
                Ok(None) | Err(_) => return false,
            }
        }
        self.tree.session_exited()
    }

    /// Phase 3a: SIGKILL whatever is still alive after the grace period.
    fn force_shutdown(&mut self) {
        if self.shutdown_complete {
            return;
        }
        // No-op without a `/proc` scan once `session_exited` saw it empty.
        let _ = self.tree.terminate_forcefully();
        if !self.finished {
            let _ = self.child.kill();
        }
    }

    /// Phase 3b: reap the shell and finish the bounded session cleanup.
    fn finish_shutdown(&mut self) {
        if self.shutdown_complete {
            return;
        }
        if !self.finished {
            let _ = self.child.wait();
            self.finished = true;
        }
        self.tree.finish_after_owner_exit();
        if let Some(worker) = self.writer_worker.take() {
            platform::reap_unit_thread(worker);
        }
        self.shutdown_complete = true;
    }
}

/// Shuts down several tab sessions concurrently: one SIGTERM wave, one shared
/// wait bounded by `TERMINAL_SHUTDOWN_GRACE`, then SIGKILL for every survivor.
/// Used for app exit so N tabs cost at most one grace period, not N.
pub(crate) fn shutdown_terminal_processes(processes: &mut [TerminalProcess]) {
    let deadline = Instant::now() + TERMINAL_SHUTDOWN_GRACE;
    for process in processes.iter_mut() {
        process.begin_shutdown();
    }
    loop {
        let mut all_exited = true;
        for process in processes.iter_mut() {
            all_exited &= process.poll_shutdown();
        }
        if all_exited || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(TERMINAL_SHUTDOWN_POLL);
    }
    for process in processes.iter_mut() {
        process.force_shutdown();
    }
    for process in processes.iter_mut() {
        process.finish_shutdown();
    }
}

impl Drop for TerminalProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn bounded_terminal_title(text: &str) -> String {
    let mut title = String::with_capacity(text.len().min(TERMINAL_TITLE_MAX_BYTES));
    for ch in text.chars() {
        if ch.is_control() {
            continue;
        }
        if title.len() + ch.len_utf8() > TERMINAL_TITLE_MAX_BYTES {
            break;
        }
        title.push(ch);
    }
    let trimmed_len = title.trim_end().len();
    title.truncate(trimmed_len);
    title
}

/// The shell owns the PTY foreground group when it is idle, so any other group is a job.
fn foreground_job_is_busy(process_group: u32, shell_pid: u32) -> bool {
    process_group != shell_pid
}

/// Labels the job a close dialog shows for a tab. A tab whose foreground group cannot be
/// read, or is the shell's own group, is idle and yields None. `read_name` runs only for
/// a busy group; when it finds no name the job is labelled "?".
fn busy_job_label(
    process_group: Option<u32>,
    shell_pid: u32,
    read_name: impl FnOnce(u32) -> Option<String>,
) -> Option<String> {
    let process_group = process_group?;
    if !foreground_job_is_busy(process_group, shell_pid) {
        return None;
    }
    Some(read_name(process_group).unwrap_or_else(|| "?".to_string()))
}

fn terminal_process_program_name(snapshot: &platform::ProcessSnapshot, shell_title: &str) -> String {
    snapshot
        .executable
        .as_deref()
        .map(terminal_shell_title)
        .or_else(|| {
            snapshot
                .args
                .first()
                .map(|arg| terminal_shell_title(Path::new(arg)))
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| shell_title.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SshDestination {
    user: Option<String>,
    host: String,
}

fn ssh_option_takes_value(option: &str) -> bool {
    matches!(
        option,
        "-B"
            | "-b"
            | "-c"
            | "-D"
            | "-E"
            | "-e"
            | "-F"
            | "-I"
            | "-i"
            | "-J"
            | "-L"
            | "-l"
            | "-m"
            | "-O"
            | "-o"
            | "-P"
            | "-p"
            | "-Q"
            | "-R"
            | "-S"
            | "-W"
            | "-w"
    )
}

fn parse_ssh_destination(args: &[OsString]) -> Option<SshDestination> {
    let mut explicit_user = None;
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].to_string_lossy();
        if arg == "--" {
            index += 1;
            break;
        }
        if arg == "-l" {
            let user = args.get(index + 1)?.to_string_lossy();
            if !user.is_empty() {
                explicit_user = Some(user.into_owned());
            }
            index += 2;
            continue;
        }
        if let Some(user) = arg.strip_prefix("-l").filter(|user| !user.is_empty()) {
            explicit_user = Some(user.to_string());
            index += 1;
            continue;
        }
        if arg.starts_with('-') && arg != "-" {
            let short_option = arg.get(..2).unwrap_or(arg.as_ref());
            index += if arg.len() == 2 && ssh_option_takes_value(short_option) {
                2
            } else {
                1
            };
            continue;
        }
        break;
    }

    let operand = args.get(index)?.to_string_lossy();
    if operand.is_empty() {
        return None;
    }
    let (operand_user, host) = operand
        .split_once('@')
        .map_or((None, operand.as_ref()), |(user, host)| {
            ((!user.is_empty()).then(|| user.to_string()), host)
        });
    if host.is_empty() {
        return None;
    }
    Some(SshDestination {
        user: operand_user.or(explicit_user),
        host: host.to_string(),
    })
}

fn terminal_title_for_snapshot(
    snapshot: &platform::ProcessSnapshot,
    initial_cwd: Option<&Path>,
    home: Option<&Path>,
    shell_title: &str,
) -> String {
    let program = terminal_process_program_name(snapshot, shell_title);
    if program.eq_ignore_ascii_case("ssh")
        && let Some(destination) = parse_ssh_destination(snapshot.args.get(1..).unwrap_or_default())
    {
        let raw = match destination.user {
            Some(user) => format!("({user}) {}", destination.host),
            None => destination.host,
        };
        return bounded_terminal_title(&raw);
    }

    let cwd = snapshot.cwd.as_deref().or(initial_cwd);
    bounded_terminal_title(&terminal_fallback_title_with_home(
        cwd,
        home,
        &program,
    ))
}

fn terminal_process_identity_changed(
    previous: &platform::ProcessSnapshot,
    current: &platform::ProcessSnapshot,
) -> bool {
    if previous.process_id != current.process_id {
        return true;
    }
    match (&previous.executable, &current.executable) {
        (Some(previous), Some(current)) => previous != current,
        _ => previous.args.first() != current.args.first(),
    }
}

fn terminal_foreground_transitioned(
    previous_process_group: Option<u32>,
    process_group: u32,
    previous: Option<&platform::ProcessSnapshot>,
    current: Option<&platform::ProcessSnapshot>,
) -> bool {
    if previous_process_group != Some(process_group) {
        return true;
    }
    match (previous, current) {
        (None, Some(_)) => true,
        (Some(previous), Some(current)) => terminal_process_identity_changed(previous, current),
        _ => false,
    }
}

/// Foreground process group of the PTY, read once per title refresh tick.
fn foreground_process_group(
    master_pty: &Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
) -> Option<u32> {
    platform::lock_recover(master_pty)
        .process_group_leader()
        .and_then(|pid| u32::try_from(pid).ok())
}

/// Records whether `process_group` leads with a `claude` process; returns
/// whether that changed. Reads `/proc` outside the title cache lock.
fn observe_claude_foreground(title_cache: &TerminalTitleCache, process_group: Option<u32>) -> bool {
    let claude = process_group
        .and_then(crate::tab_input::claude_generation)
        .is_some();
    platform::lock_recover(title_cache).set_claude_foreground(claude)
}

fn refresh_terminal_title_cache(
    process_group: Option<u32>,
    title_cache: &TerminalTitleCache,
    shell_title: &str,
    initial_cwd: Option<&Path>,
    home: Option<&Path>,
    last_process_group: &mut Option<u32>,
    snapshot: &mut Option<platform::ProcessSnapshot>,
) -> bool {
    let Some(process_group) = process_group else {
        platform::lock_recover(title_cache).observe_unchanged();
        return false;
    };

    let found = platform::foreground_process_snapshot(process_group);
    if terminal_foreground_transitioned(
        *last_process_group,
        process_group,
        snapshot.as_ref(),
        found.as_ref(),
    ) {
        let previous_was_shell = snapshot.as_ref().is_some_and(|snapshot| {
            terminal_process_program_name(snapshot, shell_title) == shell_title
        });
        let initial_observation = last_process_group.is_none();
        let detected = found.as_ref().map(|snapshot| {
            terminal_title_for_snapshot(snapshot, initial_cwd, home, shell_title)
        });
        let new_is_shell = found.as_ref().is_some_and(|snapshot| {
            terminal_process_program_name(snapshot, shell_title) == shell_title
        });
        let carry_recent_programmed =
            initial_observation || (previous_was_shell && !new_is_shell);
        *last_process_group = Some(process_group);
        *snapshot = found;
        platform::lock_recover(title_cache).observe_transition(detected, carry_recent_programmed);
        return true;
    }
    *last_process_group = Some(process_group);

    let Some(found) = found else {
        platform::lock_recover(title_cache).observe_unchanged();
        return false;
    };
    if snapshot.as_ref() != Some(&found) {
        let detected = terminal_title_for_snapshot(&found, initial_cwd, home, shell_title);
        *snapshot = Some(found);
        platform::lock_recover(title_cache).observe_detected(detected);
        return true;
    }

    platform::lock_recover(title_cache).observe_unchanged();
    false
}

fn install_terminal_title_refresh(
    master_pty: Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    title_cache: TerminalTitleCache,
    shell_title: String,
    initial_cwd: PathBuf,
    wake: Option<WakeHandle>,
) -> io::Result<(std::sync::mpsc::Sender<()>, JoinHandle<()>)> {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel();
    let worker = platform::spawn_named("ronsole-session-title", move || {
        let home = platform::user_home_dir();
        let mut last_process_group = None;
        let mut snapshot = None;
        loop {
            let group = foreground_process_group(&master_pty);
            let title_changed = refresh_terminal_title_cache(
                group,
                &title_cache,
                &shell_title,
                Some(&initial_cwd),
                home.as_deref(),
                &mut last_process_group,
                &mut snapshot,
            );
            // Evaluated on every tick, independent of the title's early returns.
            let claude_changed = observe_claude_foreground(&title_cache, group);
            if (title_changed || claude_changed)
                && let Some(wake) = wake.as_ref()
            {
                wake.wake();
            }

            match stop_rx.recv_timeout(TERMINAL_TITLE_REFRESH_INTERVAL) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        // This thread is the only writer of the flag. Clearing it after the
        // loop orders the reset after its last tick, so a session that has
        // stopped never keeps a stale `claude` marker.
        let cleared = platform::lock_recover(&title_cache).set_claude_foreground(false);
        if cleared
            && let Some(wake) = wake.as_ref()
        {
            wake.wake();
        }
    })?;
    Ok((stop_tx, worker))
}

/// Host events that the parser thread produces and the main thread drains,
/// plus the synchronized-output deadline the parser thread publishes.
///
/// Each event slot keeps only the newest value: a second OSC 52 before the
/// drain replaces the first one. `sync_deadline` is state, not an event: it is
/// rewritten after every grid change, so the main thread reads it without the
/// grid lock.
#[derive(Default)]
pub(crate) struct PendingHostEvents {
    pub(crate) clipboard: Option<String>,
    pub(crate) cwd: Option<(String, Vec<u8>)>,
    /// Set by any number of BEL bytes since the last drain.
    pub(crate) bell: bool,
    pub(crate) sync_deadline: Option<Instant>,
}

impl PendingHostEvents {
    /// Takes the pending events under a short lock and copies the published
    /// deadline, which stays in place; the caller must not hold the grid lock.
    pub(crate) fn take(shared: &Mutex<Self>) -> Self {
        let mut pending = platform::lock_recover(shared);
        Self {
            clipboard: pending.clipboard.take(),
            cwd: pending.cwd.take(),
            bell: std::mem::take(&mut pending.bell),
            sync_deadline: pending.sync_deadline,
        }
    }

    /// Reads the published synchronized-output deadline under a short lock.
    pub(crate) fn sync_deadline(shared: &Mutex<Self>) -> Option<Instant> {
        platform::lock_recover(shared).sync_deadline
    }
}

/// Host endpoints created with each terminal and handed to its I/O threads.
pub(crate) struct HostChannels {
    pub(crate) reply_tx: SyncSender<Vec<u8>>,
    pub(crate) reply_rx: Receiver<Vec<u8>>,
    pub(crate) pending: Arc<Mutex<PendingHostEvents>>,
}

/// Host side of the [`Term`] events: replies go to the PTY writer queue,
/// programmed titles go to the shared title cache, and clipboard and
/// working-directory events are parked in `pending` for the main thread.
pub(crate) struct SessionSink<'a> {
    pub(crate) reply_tx: Option<&'a SyncSender<Vec<u8>>>,
    pub(crate) title_cache: Option<&'a TerminalTitleCache>,
    pub(crate) pending: Option<&'a Arc<Mutex<PendingHostEvents>>>,
    pub(crate) wake: Option<&'a WakeHandle>,
}

impl SessionSink<'_> {
    fn notify_pending(&self) {
        if let Some(wake) = self.wake {
            wake.wake();
        }
    }
}

impl TermSink for SessionSink<'_> {
    fn reply(&mut self, bytes: &[u8]) {
        if let Some(tx) = self.reply_tx {
            let _ = tx.try_send(bytes.to_vec());
        }
    }

    fn title(&mut self, title: &str) {
        if let Some(cache) = self.title_cache {
            platform::lock_recover(cache).set_programmed(title.to_owned());
        }
    }

    fn title_reset(&mut self) {
        if let Some(cache) = self.title_cache {
            platform::lock_recover(cache).clear_programmed();
        }
    }

    fn bell(&mut self) {
        if let Some(pending) = self.pending {
            // Wake only on the false-to-true edge, so a run of BEL bytes in one
            // output batch costs a single main-loop wakeup.
            let mut events = platform::lock_recover(pending);
            if !events.bell {
                events.bell = true;
                drop(events);
                self.notify_pending();
            }
        }
    }

    fn clipboard_set(&mut self, text: String) {
        if let Some(pending) = self.pending {
            platform::lock_recover(pending).clipboard = Some(text);
            self.notify_pending();
        }
    }

    fn working_directory(&mut self, host: &str, path: &[u8]) {
        if let Some(pending) = self.pending {
            platform::lock_recover(pending).cwd = Some((host.to_owned(), path.to_vec()));
            self.notify_pending();
        }
    }
}

/// Folds the outcome of one sub-batch into the outcome of the whole output
/// batch: any grid change or new deadline stays set.
fn merge_advance_outcome(total: &mut AdvanceOutcome, step: AdvanceOutcome) {
    total.grid_changed |= step.grid_changed;
    total.new_deadline |= step.new_deadline;
}

/// Feeds one output batch and reports what it changed. The content generation
/// moves only when the grid changed, so a batch held by synchronized output
/// does not repaint.
fn advance_terminal_output_batch(
    grid: &mut Term,
    sink: &mut SessionSink<'_>,
    chunks: &[OutputChunk],
    now: Instant,
) -> AdvanceOutcome {
    let mut outcome = AdvanceOutcome::default();
    // Held bytes past their deadline are replayed before newer bytes are
    // parsed, so a late batch cannot extend a synchronized-output block.
    if grid.sync_deadline().is_some_and(|deadline| now >= deadline) {
        let flushed = flush_expired_sync(grid, sink, now);
        merge_advance_outcome(&mut outcome, flushed);
    }
    for chunk in chunks {
        let step = grid.advance(&chunk.bytes[..chunk.len], now, sink);
        merge_advance_outcome(&mut outcome, step);
    }
    if outcome.grid_changed {
        grid.mark_changed();
    }
    publish_sync_deadline(grid, sink.pending);
    outcome
}

/// Copies the grid's held synchronized-output deadline into `pending`, so the
/// main thread reads it without the grid lock. Callers hold the grid lock and
/// take the pending lock after it, the same order as [`SessionSink`].
pub(crate) fn publish_sync_deadline(
    grid: &Term,
    pending: Option<&Arc<Mutex<PendingHostEvents>>>,
) {
    if let Some(pending) = pending {
        platform::lock_recover(pending).sync_deadline = grid.sync_deadline();
    }
}

/// Replays the held synchronized-output bytes whose deadline has passed at
/// `now`, then publishes the deadline that is left. Callers hold the screen
/// and grid locks and wake the main loop from the returned outcome.
pub(crate) fn flush_expired_sync(
    grid: &mut Term,
    sink: &mut SessionSink<'_>,
    now: Instant,
) -> AdvanceOutcome {
    let outcome = grid.flush_expired(now, sink);
    if outcome.grid_changed {
        grid.mark_changed();
    }
    publish_sync_deadline(grid, sink.pending);
    outcome
}

/// Receives the next output item. While synchronized output holds bytes, the
/// wait ends at their deadline and `flush` replays them, so an idle parser
/// releases the block without the main thread. `flush` returns the deadline
/// still held afterwards, which becomes the new `held_until`.
fn recv_output_chunk<T>(
    rx: &Receiver<T>,
    held_until: &mut Option<Instant>,
    mut flush: impl FnMut(Instant) -> Option<Instant>,
) -> Option<T> {
    loop {
        let Some(deadline) = *held_until else {
            return rx.recv().ok();
        };
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(item) => return Some(item),
            Err(RecvTimeoutError::Timeout) => *held_until = flush(Instant::now()),
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Wakes the main loop according to a batch outcome: a grid change asks for a
/// frame, a new synchronized-output deadline only needs the loop to re-arm its
/// timeout, and anything else wakes nothing.
pub(crate) fn notify_main_loop(wake: Option<&WakeHandle>, outcome: AdvanceOutcome) {
    let Some(wake) = wake else {
        return;
    };
    if outcome.grid_changed {
        wake.wake();
    } else if outcome.new_deadline {
        wake.wake_timer();
    }
}

#[inline]
fn terminal_output_batch_action(
    chunk_count: usize,
    elapsed: Duration,
) -> TerminalOutputBatchAction {
    if elapsed >= TERMINAL_OUTPUT_BATCH_MAX {
        TerminalOutputBatchAction::FlushAndRedraw
    } else if chunk_count >= TERMINAL_OUTPUT_BATCH_MAX_CHUNKS {
        TerminalOutputBatchAction::FlushAndContinue
    } else {
        TerminalOutputBatchAction::Collect
    }
}

#[inline]
fn terminal_output_batch_receive_timeout(elapsed: Duration) -> Option<Duration> {
    TERMINAL_OUTPUT_BATCH_MAX
        .checked_sub(elapsed)
        .filter(|remaining| !remaining.is_zero())
        .map(|remaining| remaining.min(TERMINAL_OUTPUT_BATCH_IDLE))
}

fn finish_terminal_output_stream(
    grid: &mut Term,
    sink: &mut SessionSink<'_>,
    now: Instant,
) -> AdvanceOutcome {
    if grid.presentation_ready() {
        return AdvanceOutcome::default();
    }

    let outcome = grid.advance(TERMINAL_EXIT_BEFORE_OUTPUT, now, sink);
    // The exit notice always repaints, even when synchronized output held its
    // bytes, so the batch reports a grid change and the main loop gets a frame.
    // Only the content generation moves here; `dirty` keeps its prior value.
    let was_dirty = grid.dirty();
    grid.mark_changed();
    if !was_dirty {
        grid.set_dirty(false);
    }
    publish_sync_deadline(grid, sink.pending);
    AdvanceOutcome {
        grid_changed: true,
        ..outcome
    }
}

fn terminal_read_buffer() -> Vec<u8> {
    vec![0; TERMINAL_READ_BUFFER_SIZE]
}

fn recycle_output_chunks(
    chunks: &mut Vec<OutputChunk>,
    recycle_tx: &std::sync::mpsc::SyncSender<Vec<u8>>,
) {
    for chunk in chunks.drain(..) {
        let _ = recycle_tx.try_send(chunk.bytes);
    }
}

pub(crate) fn terminal_reply_channel() -> (
    std::sync::mpsc::SyncSender<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    std::sync::mpsc::sync_channel(TERMINAL_REPLY_QUEUE_CAPACITY)
}

fn install_terminal_io_threads(
    grid: &Arc<Mutex<Term>>,
    bridge_screen_lock: Arc<Mutex<()>>,
    reader: Box<dyn Read + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    wake: Option<WakeHandle>,
    title_cache: TerminalTitleCache,
    host: HostChannels,
) -> io::Result<JoinHandle<()>> {
    let HostChannels {
        reply_tx,
        reply_rx,
        pending,
    } = host;
    let (tx, rx) = std::sync::mpsc::sync_channel::<OutputChunk>(TERMINAL_OUTPUT_QUEUE_CAPACITY);
    let (recycle_tx, recycle_rx) =
        std::sync::mpsc::sync_channel::<Vec<u8>>(TERMINAL_OUTPUT_QUEUE_CAPACITY);
    let reader_finished = Arc::new(AtomicBool::new(false));

    let parser_grid = grid.clone();
    let parser_bridge_screen_lock = bridge_screen_lock;
    let parser_reader_finished = reader_finished.clone();
    platform::spawn_named("ronsole-terminal-parser", move || {
        let mut batch = Vec::with_capacity(8);
        // Deadline of the held synchronized-output bytes as this thread last
        // saw it; the grid is only changed by this thread or by the main
        // thread's fallback, which leaves the deadline at most stale.
        let mut held_until: Option<Instant> = None;
        let mut flush_held_sync = |now: Instant| -> Option<Instant> {
            let (outcome, deadline) = {
                let _screen_guard = platform::lock_recover(&parser_bridge_screen_lock);
                let mut grid = platform::lock_recover(&parser_grid);
                let mut sink = SessionSink {
                    reply_tx: Some(&reply_tx),
                    title_cache: Some(&title_cache),
                    pending: Some(&pending),
                    wake: wake.as_ref(),
                };
                let outcome = flush_expired_sync(&mut grid, &mut sink, now);
                (outcome, grid.sync_deadline())
            };
            notify_main_loop(wake.as_ref(), outcome);
            deadline
        };
        let mut first = recv_output_chunk(&rx, &mut held_until, &mut flush_held_sync);

        while let Some(first_chunk) = first {
            batch.push(first_chunk);
            let started = Instant::now();
            // The main loop is woken once for the whole output batch, after
            // its last sub-batch, so a long batch does not repaint mid-way.
            let mut batch_outcome = AdvanceOutcome::default();
            loop {
                let action = loop {
                    let action = terminal_output_batch_action(batch.len(), started.elapsed());
                    if action != TerminalOutputBatchAction::Collect {
                        break action;
                    }
                    let Some(timeout) = terminal_output_batch_receive_timeout(started.elapsed())
                    else {
                        break TerminalOutputBatchAction::FlushAndRedraw;
                    };
                    match rx.recv_timeout(timeout) {
                        Ok(next) => batch.push(next),
                        Err(_) => break TerminalOutputBatchAction::FlushAndRedraw,
                    }
                };

                let (outcome, deadline) = {
                    let _screen_guard = platform::lock_recover(&parser_bridge_screen_lock);
                    let mut grid = platform::lock_recover(&parser_grid);
                    let mut sink = SessionSink {
                        reply_tx: Some(&reply_tx),
                        title_cache: Some(&title_cache),
                        pending: Some(&pending),
                        wake: wake.as_ref(),
                    };
                    let outcome =
                        advance_terminal_output_batch(&mut grid, &mut sink, &batch, Instant::now());
                    (outcome, grid.sync_deadline())
                };
                held_until = deadline;
                recycle_output_chunks(&mut batch, &recycle_tx);
                merge_advance_outcome(&mut batch_outcome, outcome);

                match action {
                    TerminalOutputBatchAction::FlushAndContinue => {
                        let Some(timeout) =
                            terminal_output_batch_receive_timeout(started.elapsed())
                        else {
                            break;
                        };
                        match rx.recv_timeout(timeout) {
                            Ok(next) => batch.push(next),
                            Err(_) => break,
                        }
                    }
                    TerminalOutputBatchAction::Collect
                    | TerminalOutputBatchAction::FlushAndRedraw => {
                        break;
                    }
                }
            }
            notify_main_loop(wake.as_ref(), batch_outcome);
            first = recv_output_chunk(&rx, &mut held_until, &mut flush_held_sync);
        }

        if parser_reader_finished.load(Ordering::Acquire) {
            let outcome = {
                let _screen_guard = platform::lock_recover(&parser_bridge_screen_lock);
                let mut grid = platform::lock_recover(&parser_grid);
                let mut sink = SessionSink {
                    reply_tx: Some(&reply_tx),
                    title_cache: Some(&title_cache),
                    pending: Some(&pending),
                    wake: wake.as_ref(),
                };
                finish_terminal_output_stream(&mut grid, &mut sink, Instant::now())
            };
            notify_main_loop(wake.as_ref(), outcome);
        }
    })
    .map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to spawn terminal parser: {error}"),
        )
    })?;

    // Kept by the caller so session teardown can reap it; the reader and
    // parser threads stay detached as before.
    let writer_worker = platform::spawn_named("ronsole-terminal-writer", move || {
        while let Ok(message) = reply_rx.recv() {
            let Ok(mut writer) = writer.lock() else {
                break;
            };
            if writer.write_all(&message).is_err() || writer.flush().is_err() {
                break;
            }
        }
    })
    .map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to spawn terminal writer: {error}"),
        )
    })?;

    platform::spawn_named("ronsole-terminal-reader", move || {
        let mut reader = reader;
        let mut buffer = terminal_read_buffer();
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let chunk = OutputChunk {
                        bytes: buffer,
                        len: read,
                    };
                    if tx.send(chunk).is_err() {
                        break;
                    }
                    buffer = recycle_rx.try_recv().unwrap_or_else(|_| terminal_read_buffer());
                }
                Err(_) => break,
            }
        }
        reader_finished.store(true, Ordering::Release);
    })
    .map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to spawn terminal reader: {error}"),
        )
    })?;

    Ok(writer_worker)
}

fn terminal_launch_command(
    launch: TerminalLaunchSpec,
    tab_token: &TabToken,
) -> io::Result<(CommandBuilder, PathBuf, String, String)> {
    let cwd = match launch.working_directory {
        Some(cwd) => cwd,
        None => terminal_launch_directory(
            launch.inherited_directory.as_deref(),
            &terminal_working_directory()?,
        ),
    };
    let (mut command, process_title, spawn_target) = if launch.command.is_empty() {
        let shell = resolve_terminal_shell(launch.shell.as_deref())?;
        // An explicit override names its program in the spawn error; the default
        // shell keeps the plain wording.
        let spawn_target = if launch.shell.is_some() {
            format!("terminal shell {}", shell.executable.display())
        } else {
            "terminal shell".to_string()
        };
        (
            CommandBuilder::new(&shell.executable),
            shell.title,
            spawn_target,
        )
    } else {
        let process_title = terminal_shell_title(Path::new(&launch.command[0]));
        (
            CommandBuilder::from_argv(launch.command),
            process_title,
            "terminal command".to_string(),
        )
    };
    command.cwd(cwd.as_os_str());
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    // Never leak a token inherited from Ronsole's own environment.
    if tab_token.as_str().is_empty() {
        command.env_remove(TERMINAL_TAB_TOKEN_ENV);
    } else {
        command.env(TERMINAL_TAB_TOKEN_ENV, tab_token.as_str());
    }
    Ok((command, cwd, process_title, spawn_target))
}

pub(crate) fn resolve_terminal_shell(
    shell_override: Option<&Path>,
) -> io::Result<TerminalShellSpec> {
    resolve_terminal_shell_with(
        shell_override,
        std::env::var_os("SHELL"),
        platform::resolve_executable,
    )
}

/// An explicit `shell_override` is the only candidate, so a missing program fails
/// instead of falling back to `$SHELL` or `/bin/bash`.
fn resolve_terminal_shell_with(
    shell_override: Option<&Path>,
    shell_env: Option<OsString>,
    mut resolve: impl FnMut(&OsStr) -> Option<PathBuf>,
) -> io::Result<TerminalShellSpec> {
    let candidates = match shell_override {
        Some(program) => vec![program.as_os_str().to_os_string()],
        None => terminal_shell_candidates_with(shell_env),
    };
    for candidate in candidates {
        if let Some(executable) = resolve(&candidate) {
            let title = terminal_shell_title(&executable);
            return Ok(TerminalShellSpec { executable, title });
        }
    }

    let message = match shell_override {
        Some(program) => format!("no supported terminal shell was found: {}", program.display()),
        None => "no supported terminal shell was found".to_string(),
    };
    Err(io::Error::new(io::ErrorKind::NotFound, message))
}

fn terminal_shell_candidates_with(shell_env: Option<OsString>) -> Vec<OsString> {
    let mut candidates = Vec::with_capacity(3);
    if let Some(shell) = shell_env.filter(|value| !value.is_empty()) {
        candidates.push(shell);
    }
    for fallback in [OsString::from("/bin/bash"), OsString::from("/bin/sh")] {
        if !candidates.iter().any(|candidate| candidate == &fallback) {
            candidates.push(fallback);
        }
    }
    candidates
}

fn terminal_working_directory() -> io::Result<PathBuf> {
    terminal_working_directory_with(platform::user_home_dir())
}

fn terminal_working_directory_with(home: Option<PathBuf>) -> io::Result<PathBuf> {
    home.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "$HOME is not available"))
}

/// Picks the folder for a session without an explicit directory: the
/// inherited directory when it still exists, otherwise `home`.
fn terminal_launch_directory(candidate: Option<&Path>, home: &Path) -> PathBuf {
    match candidate {
        Some(candidate) if candidate.is_dir() => candidate.to_path_buf(),
        _ => home.to_path_buf(),
    }
}

fn terminal_shell_title(path: &Path) -> String {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "terminal".to_string())
}

pub(crate) fn terminal_fallback_title(cwd: Option<&Path>, shell_title: &str) -> String {
    let home = platform::user_home_dir();
    terminal_fallback_title_with_home(cwd, home.as_deref(), shell_title)
}

fn terminal_fallback_title_with_home(
    cwd: Option<&Path>,
    home: Option<&Path>,
    shell_title: &str,
) -> String {
    let Some(cwd) = cwd else {
        return shell_title.to_string();
    };

    let cwd_label = if home.is_some_and(|home| cwd == home) {
        "~".to_string()
    } else if let Some(name) = cwd.file_name().filter(|name| !name.is_empty()) {
        name.to_string_lossy().into_owned()
    } else {
        let root = cwd.as_os_str().to_string_lossy();
        if root.is_empty() {
            "~".to_string()
        } else {
            root.into_owned()
        }
    };

    format!("{cwd_label} : {shell_title}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_claude_foreground_tracks_claude_leader_and_reports_only_changes() {
        use crate::tab_input::test_support::{TestDir, spawn_shell};

        let dir = TestDir::new("claude-foreground");
        let claude = dir.shell_named("claude");
        let leader = spawn_shell(&claude, "sleep 30; true");
        let pid = leader.pid();
        let cache: TerminalTitleCache = Arc::new(Mutex::new(TerminalTitleState::new(
            "terminal".to_string(),
        )));

        let deadline = Instant::now() + Duration::from_secs(2);
        while crate::tab_input::claude_generation(pid).is_none() {
            assert!(Instant::now() < deadline, "pid {pid} never reached comm `claude`");
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(observe_claude_foreground(&cache, Some(pid)));
        assert!(platform::lock_recover(&cache).claude_foreground());
        assert!(!observe_claude_foreground(&cache, Some(pid)));
        assert!(platform::lock_recover(&cache).claude_foreground());
        assert!(observe_claude_foreground(&cache, Some(std::process::id())));
        assert!(!platform::lock_recover(&cache).claude_foreground());
        assert!(!observe_claude_foreground(&cache, None));
        assert!(!platform::lock_recover(&cache).claude_foreground());
    }

    #[test]
    fn set_claude_foreground_reports_change() {
        let mut state = TerminalTitleState::new("terminal".to_string());
        assert!(state.set_claude_foreground(true));
        assert!(!state.set_claude_foreground(true));
    }

    #[test]
    fn merge_advance_outcome_keeps_any_grid_change_and_deadline() {
        let quiet = AdvanceOutcome::default();
        let changed = AdvanceOutcome {
            grid_changed: true,
            new_deadline: false,
        };
        let deadline = AdvanceOutcome {
            grid_changed: false,
            new_deadline: true,
        };

        let mut total = AdvanceOutcome::default();
        merge_advance_outcome(&mut total, quiet);
        assert_eq!(total, AdvanceOutcome::default(), "false+false stays false");

        merge_advance_outcome(&mut total, changed);
        assert!(total.grid_changed, "true+false sets grid_changed");
        assert!(!total.new_deadline);

        merge_advance_outcome(&mut total, quiet);
        assert!(total.grid_changed, "a later quiet sub-batch keeps grid_changed");

        merge_advance_outcome(&mut total, deadline);
        assert!(total.grid_changed);
        assert!(total.new_deadline, "new_deadline from a later sub-batch is kept");

        merge_advance_outcome(&mut total, quiet);
        assert!(
            total.new_deadline,
            "a later quiet sub-batch keeps new_deadline"
        );
    }

    #[test]
    fn foreground_job_is_busy_only_when_group_differs_from_shell() {
        // Idle shell owns the foreground group. Background jobs (`&`) do not move it.
        assert!(!foreground_job_is_busy(4242, 4242));
        // A foreground job runs in its own process group.
        assert!(foreground_job_is_busy(4343, 4242));
    }

    #[test]
    fn busy_job_label_treats_unreadable_or_shell_group_as_idle() {
        let mut name_reads = 0usize;
        // No readable foreground group: the tab is idle and the name is never read.
        assert_eq!(
            busy_job_label(None, 4242, |_| {
                name_reads += 1;
                Some("vim".to_string())
            }),
            None
        );
        // The shell owns its own group: idle.
        assert_eq!(
            busy_job_label(Some(4242), 4242, |_| {
                name_reads += 1;
                Some("bash".to_string())
            }),
            None
        );
        assert_eq!(name_reads, 0);
    }

    #[test]
    fn busy_job_label_names_a_job_or_marks_an_unreadable_one() {
        assert_eq!(
            busy_job_label(Some(4343), 4242, |group| {
                assert_eq!(group, 4343);
                Some("vim".to_string())
            }),
            Some("vim".to_string())
        );
        // A busy group whose name cannot be read is still busy, shown as "?".
        assert_eq!(
            busy_job_label(Some(4343), 4242, |_| None),
            Some("?".to_string())
        );
    }

    #[derive(Clone, Copy, Debug)]
    struct TestLinuxStat {
        process_group: i32,
        session_id: i32,
        state: u8,
    }

    fn test_linux_stat(process_id: u32) -> Option<TestLinuxStat> {
        let stat = std::fs::read(format!("/proc/{process_id}/stat")).ok()?;
        let command_end = stat.iter().rposition(|byte| *byte == b')')?;
        let fields = std::str::from_utf8(stat.get(command_end + 1..)?).ok()?;
        let mut fields = fields.split_ascii_whitespace();
        let state = *fields.next()?.as_bytes().first()?;
        fields.next()?;
        let process_group = fields.next()?.parse().ok()?;
        let session_id = fields.next()?.parse().ok()?;
        Some(TestLinuxStat {
            process_group,
            session_id,
            state,
        })
    }

    fn process_is_live(process_id: u32) -> bool {
        test_linux_stat(process_id).is_some_and(|stat| stat.state != b'Z')
    }

    fn wait_for_process_gone(process_id: u32, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if !process_is_live(process_id) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        !process_is_live(process_id)
    }

    struct TempPidPath(PathBuf);

    impl Drop for TempPidPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn unique_temp_path(tag: &str) -> TempPidPath {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        TempPidPath(std::env::temp_dir().join(format!(
            "ronsole-{tag}-{}-{unique}",
            std::process::id()
        )))
    }

    fn read_pid_file(path: &TempPidPath) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(text) = std::fs::read_to_string(&path.0)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                return pid;
            }
            assert!(Instant::now() < deadline, "session job did not report its pid");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// `bash -c` session leader that runs `job` as a background job in its own
    /// process group of the same session, then exits on SIGTERM. The job writes
    /// its pid to the returned path after installing its own traps.
    fn spawn_test_session_with_job(job: &str) -> (TerminalProcess, u32, u32, TempPidPath) {
        let pid_path = unique_temp_path("shutdown-job.pid");
        let job = job.replace("PIDFILE", &pid_path.0.display().to_string());
        let mut command = CommandBuilder::new("/bin/bash");
        command.arg("-c");
        command.arg(format!("trap 'exit 0' TERM; set -m; sh -c '{job}' & wait"));
        let (process, shell_pid) = spawn_test_session(command);
        let job_pid = read_pid_file(&pid_path);
        let shell_stat = test_linux_stat(shell_pid).unwrap();
        let job_stat = test_linux_stat(job_pid).unwrap();
        assert_ne!(shell_stat.process_group, job_stat.process_group);
        assert_eq!(shell_stat.session_id, job_stat.session_id);
        (process, shell_pid, job_pid, pid_path)
    }

    fn spawn_test_session(command: CommandBuilder) -> (TerminalProcess, u32) {
        let pair = NativePtySystem::default()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let child = pair.slave.spawn_command(command).unwrap();
        drop(pair.slave);

        let shell_pid = child.process_id().unwrap();
        let tree = ProcessTree::attach_process_id(shell_pid).unwrap();
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let master_pty = Arc::new(Mutex::new(pair.master));
        let process = TerminalProcess {
            writer,
            master_pty,
            child,
            shell_pid,
            tree,
            title_stop_tx: None,
            title_worker: None,
            writer_worker: None,
            finished: false,
            shutdown_complete: false,
        };
        (process, shell_pid)
    }

    fn spawn_test_bash_with_background_job() -> (TerminalProcess, u32, u32, TempPidPath) {
        let mut command = CommandBuilder::new("/bin/bash");
        command.arg("--noprofile");
        command.arg("--norc");
        command.arg("-i");
        command.env("TERM", "xterm-256color");
        let (process, shell_pid) = spawn_test_session(command);

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let pid_path = TempPidPath(std::env::temp_dir().join(format!(
            "ronsole-cleanup-job-{}-{unique}.pid",
            std::process::id()
        )));
        let command = format!(
            "set -m; sleep 30 & job=$!; disown; printf '%s\\n' \"$job\" > {};\r",
            pid_path.0.display()
        );
        process.write_input(command.as_bytes()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let job_pid = loop {
            if let Ok(text) = std::fs::read_to_string(&pid_path.0)
                && let Ok(pid) = text.trim().parse::<u32>()
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "bash did not report background job pid");
            std::thread::sleep(Duration::from_millis(10));
        };
        let shell_stat = test_linux_stat(shell_pid).unwrap();
        let job_stat = test_linux_stat(job_pid).unwrap();
        assert_ne!(shell_stat.process_group, job_stat.process_group);
        assert_eq!(shell_stat.session_id, job_stat.session_id);
        (process, shell_pid, job_pid, pid_path)
    }

    /// Feeds one batch of output through the parser's batch path, so the
    /// deadline is published exactly as the parser thread publishes it.
    fn feed_parser_batch(
        grid: &mut Term,
        pending: &Arc<Mutex<PendingHostEvents>>,
        bytes: &[u8],
        now: Instant,
    ) {
        let mut sink = SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: Some(pending),
            wake: None,
        };
        advance_terminal_output_batch(grid, &mut sink, &[output_chunk(bytes)], now);
    }

    #[test]
    fn parser_releases_held_sync_block_at_deadline_without_main_thread() {
        let grid = Mutex::new(test_term(40, 3));
        let pending = Arc::new(Mutex::new(PendingHostEvents::default()));
        let now = Instant::now();
        feed_parser_batch(
            &mut platform::lock_recover(&grid),
            &pending,
            b"\x1b[?2026hX",
            now,
        );
        // The 150 ms hold window comes from the synchronized-output spec (5.4).
        let deadline =
            PendingHostEvents::sync_deadline(&pending).expect("held bytes publish a deadline");
        assert_eq!(deadline, now + Duration::from_millis(150));

        let (tx, rx) = std::sync::mpsc::sync_channel::<u8>(1);
        let mut held_until = Some(deadline);
        let mut flushed_at = None;
        // Only the parser's own wait runs here: the flush closure is the
        // parser-side replay, and the channel gets one item after it.
        let next = recv_output_chunk(&rx, &mut held_until, |at| {
            flushed_at = Some(at);
            {
                let mut grid = platform::lock_recover(&grid);
                let mut sink = SessionSink {
                    reply_tx: None,
                    title_cache: None,
                    pending: Some(&pending),
                    wake: None,
                };
                flush_expired_sync(&mut grid, &mut sink, at);
            }
            tx.send(7).expect("channel has room for one item");
            PendingHostEvents::sync_deadline(&pending)
        });

        assert_eq!(next, Some(7));
        assert!(flushed_at.expect("the wait ends at the deadline") >= deadline);
        assert_eq!(held_until, None);
        assert!(PendingHostEvents::sync_deadline(&pending).is_none());
        let guard = platform::lock_recover(&grid);
        assert_eq!(guard.visible_row(0).expect("screen row")[0].ch(), 'X');
    }

    #[test]
    fn late_parser_batch_replays_expired_sync_block_before_newer_bytes() {
        let mut grid = test_term(40, 3);
        let pending = Arc::new(Mutex::new(PendingHostEvents::default()));
        let now = Instant::now();
        feed_parser_batch(&mut grid, &pending, b"\x1b[?2026hA", now);
        // The batch arrives after the 150 ms window: the held A is replayed
        // first, so the newer B lands after it.
        feed_parser_batch(&mut grid, &pending, b"B", now + Duration::from_secs(1));

        let row = grid.visible_row(0).expect("screen row");
        assert_eq!((row[0].ch(), row[1].ch()), ('A', 'B'));
        assert!(PendingHostEvents::sync_deadline(&pending).is_none());
    }

    fn output_chunk(bytes: &[u8]) -> OutputChunk {
        let mut buffer = vec![0; TERMINAL_READ_BUFFER_SIZE];
        buffer[..bytes.len()].copy_from_slice(bytes);
        OutputChunk {
            bytes: buffer,
            len: bytes.len(),
        }
    }

    fn test_term(cols: usize, rows: usize) -> Term {
        Term::new(ronsole_term::TermConfig {
            cols,
            rows,
            scrollback_lines: 10_000,
            identity: ronsole_term::TermIdentity {
                name: "ronsole-test",
                version: "0",
            },
            palette: ronsole_term::TermPalette {
                foreground: ronsole_term::Rgb {
                    r: 0xe5,
                    g: 0xe5,
                    b: 0xe5,
                },
                background: ronsole_term::Rgb { r: 0, g: 0, b: 0 },
                cursor: ronsole_term::Rgb {
                    r: 0xe5,
                    g: 0xe5,
                    b: 0xe5,
                },
                ansi: [ronsole_term::Rgb { r: 0, g: 0, b: 0 }; 16],
                bold_foreground: None,
            },
        })
    }

    fn parser_grid() -> (Term, TerminalTitleCache) {
        let cache = Arc::new(Mutex::new(TerminalTitleState::new("fallback".to_string())));
        (test_term(8, 2), cache)
    }

    fn process_snapshot(program: &str, cwd: &str, args: &[&str]) -> platform::ProcessSnapshot {
        platform::ProcessSnapshot {
            process_id: 42,
            executable: Some(PathBuf::from(format!("/usr/bin/{program}"))),
            cwd: Some(PathBuf::from(cwd)),
            args: args.iter().map(OsString::from).collect(),
        }
    }

    fn resolved_title(state: &TerminalTitleState) -> String {
        let mut title = String::new();
        state.write_resolved(&mut title);
        title
    }

    #[test]
    fn home_is_the_only_terminal_working_directory_policy() {
        assert_eq!(
            terminal_working_directory_with(Some(PathBuf::from("/home/reyan"))).unwrap(),
            PathBuf::from("/home/reyan")
        );
        assert!(terminal_working_directory_with(None).is_err());
        let source = include_str!("terminal_process.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        assert!(!production.contains("current_dir"));
        assert!(!production.contains("workspace"));
    }

    #[test]
    fn shell_resolution_prefers_valid_shell_then_linux_fallbacks() {
        let fish = resolve_terminal_shell_with(None, Some(OsString::from("/usr/bin/fish")), |candidate| {
            (candidate == OsStr::new("/usr/bin/fish")).then(|| PathBuf::from("/usr/bin/fish"))
        })
        .unwrap();
        assert_eq!(fish.executable, PathBuf::from("/usr/bin/fish"));
        assert_eq!(fish.title, "fish");

        let bash = resolve_terminal_shell_with(None, Some(OsString::from("/missing/shell")), |candidate| {
            (candidate == OsStr::new("/bin/bash")).then(|| PathBuf::from("/bin/bash"))
        })
        .unwrap();
        assert_eq!(bash.executable, PathBuf::from("/bin/bash"));
        assert_eq!(
            terminal_shell_candidates_with(Some(OsString::from("/bin/bash"))),
            [OsString::from("/bin/bash"), OsString::from("/bin/sh")]
        );
    }

    #[test]
    fn shell_override_is_used_without_fallback() {
        // The fallback candidates would resolve here, so a success would mean the
        // override silently fell back to `$SHELL` or `/bin/bash`.
        let error = resolve_terminal_shell_with(
            Some(Path::new("/nonexistent/sh")),
            Some(OsString::from("/usr/bin/fish")),
            |candidate| (candidate != OsStr::new("/nonexistent/sh")).then(|| PathBuf::from(candidate)),
        )
        .err()
        .expect("missing override must not fall back to another shell");
        assert!(error.to_string().contains("/nonexistent/sh"), "{error}");

        let sh = resolve_terminal_shell_with(
            Some(Path::new("/bin/sh")),
            Some(OsString::from("/usr/bin/fish")),
            |candidate| Some(PathBuf::from(candidate)),
        )
        .unwrap();
        assert_eq!(sh.executable, PathBuf::from("/bin/sh"));
    }

    #[test]
    fn shell_override_names_program_in_spawn_target() {
        let launch = TerminalLaunchSpec {
            shell: Some(PathBuf::from("/bin/sh")),
            working_directory: Some(std::env::temp_dir()),
            inherited_directory: None,
            command: Vec::new(),
            hold: false,
            bridge_launch_id: None,
        };
        let (_command, _cwd, _title, spawn_target) =
            terminal_launch_command(launch, &TabToken::generate()).unwrap();
        // The spawn error reads "failed to spawn {spawn_target}: ...", so it must name the program.
        assert_eq!(spawn_target, "terminal shell /bin/sh");
    }

    #[test]
    fn terminal_environment_advertises_implemented_truecolor() {
        let source = include_str!("terminal_process.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        assert!(production.contains("command.env(\"TERM\", \"xterm-256color\")"));
        assert!(production.contains("command.env(\"COLORTERM\", \"truecolor\")"));
    }

    #[test]
    fn direct_launch_builder_preserves_raw_argv_cwd_title_and_terminal_env() {
        use std::os::unix::ffi::OsStringExt;

        let cwd = PathBuf::from("/tmp/ronsole-direct-launch");
        let raw_arg = OsString::from_vec(vec![b'r', b'a', b'w', 0xff, b'x']);
        let argv = vec![
            OsString::from("/usr/bin/example-tool"),
            OsString::from("arg one"),
            OsString::from("--something"),
            OsString::from("русский/utf8"),
            raw_arg,
        ];
        let launch = TerminalLaunchSpec {
            shell: None,
            working_directory: Some(cwd.clone()),
            inherited_directory: None,
            command: argv.clone(),
            hold: true,
            bridge_launch_id: None,
        };

        let (command, resolved_cwd, title, spawn_target) =
            terminal_launch_command(launch, &TabToken::generate()).unwrap();

        assert_eq!(resolved_cwd, cwd);
        assert_eq!(command.get_argv(), &argv);
        assert_eq!(
            command.get_cwd().map(|cwd| PathBuf::from(cwd.as_os_str())),
            Some(cwd)
        );
        assert_eq!(command.get_env("TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(command.get_env("COLORTERM"), Some(OsStr::new("truecolor")));
        assert_eq!(title, "example-tool");
        assert_eq!(spawn_target, "terminal command");
    }

    #[test]
    fn tab_token_is_exported_unique_hex_and_redacted_in_debug() {
        let first = TabToken::generate();
        let second = TabToken::generate();
        for token in [&first, &second] {
            assert_eq!(token.as_str().len(), 32);
            assert!(
                token
                    .as_str()
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            );
            let debug = format!("{token:?}");
            assert!(!debug.contains(token.as_str()), "{debug}");
            assert!(debug.contains("<redacted>"), "{debug}");
        }
        assert_ne!(first.as_str(), second.as_str());

        let launch = TerminalLaunchSpec {
            shell: None,
            working_directory: Some(PathBuf::from("/tmp")),
            inherited_directory: None,
            command: vec![OsString::from("/bin/true")],
            hold: false,
            bridge_launch_id: None,
        };
        let (command, ..) = terminal_launch_command(launch, &first).unwrap();
        assert_eq!(
            command.get_env(TERMINAL_TAB_TOKEN_ENV),
            Some(OsStr::new(first.as_str()))
        );
        assert_eq!(TERMINAL_TAB_TOKEN_ENV, "RONSOLE_TAB_TOKEN");
    }

    #[test]
    fn terminal_output_queue_is_bounded_and_buffers_are_recyclable() {
        assert_eq!(TERMINAL_OUTPUT_QUEUE_CAPACITY, 16);
        assert_eq!(TERMINAL_OUTPUT_BATCH_MAX_CHUNKS, 32);
        assert_eq!(TERMINAL_READ_BUFFER_SIZE, 65_536);
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(1);
        let mut chunks = vec![output_chunk(b"abc")];
        recycle_output_chunks(&mut chunks, &tx);
        assert!(chunks.is_empty());
        assert_eq!(rx.recv().unwrap().len(), TERMINAL_READ_BUFFER_SIZE);
    }

    #[test]
    fn terminal_output_chunk_cap_flushes_parser_without_forcing_intermediate_redraw() {
        assert_eq!(
            terminal_output_batch_action(TERMINAL_OUTPUT_BATCH_MAX_CHUNKS - 1, Duration::ZERO),
            TerminalOutputBatchAction::Collect
        );
        assert_eq!(
            terminal_output_batch_action(TERMINAL_OUTPUT_BATCH_MAX_CHUNKS, Duration::ZERO),
            TerminalOutputBatchAction::FlushAndContinue
        );
        assert_eq!(
            terminal_output_batch_action(
                TERMINAL_OUTPUT_BATCH_MAX_CHUNKS,
                TERMINAL_OUTPUT_BATCH_MAX,
            ),
            TerminalOutputBatchAction::FlushAndRedraw
        );
    }

    #[test]
    fn terminal_output_idle_wait_is_capped_by_visual_batch_deadline() {
        assert_eq!(
            terminal_output_batch_receive_timeout(Duration::ZERO),
            Some(TERMINAL_OUTPUT_BATCH_IDLE)
        );
        assert_eq!(
            terminal_output_batch_receive_timeout(Duration::from_millis(30)),
            Some(Duration::from_millis(2))
        );
        assert_eq!(
            terminal_output_batch_receive_timeout(TERMINAL_OUTPUT_BATCH_MAX),
            None
        );
    }

    #[test]
    fn terminal_reply_queue_has_explicit_bounded_capacity() {
        assert_eq!(TERMINAL_REPLY_QUEUE_CAPACITY, 32);
        let (tx, rx) = terminal_reply_channel();
        for index in 0..TERMINAL_REPLY_QUEUE_CAPACITY {
            tx.try_send(vec![u8::try_from(index).unwrap()]).unwrap();
        }
        assert!(matches!(
            tx.try_send(vec![0xff]),
            Err(std::sync::mpsc::TrySendError::Full(_))
        ));
        assert_eq!(rx.try_iter().count(), TERMINAL_REPLY_QUEUE_CAPACITY);
    }

    #[test]
    fn terminal_writer_thread_exits_once_reply_queue_closes() {
        // Source: the writer loop ends when `reply_rx.recv()` fails, which
        // happens once the parser drops `reply_tx` after the reader hits EOF.
        let grid = Arc::new(Mutex::new(test_term(8, 2)));
        let cache: TerminalTitleCache = Arc::new(Mutex::new(TerminalTitleState::new(
            "fallback".to_string(),
        )));
        let (reply_tx, reply_rx) = terminal_reply_channel();
        let host = HostChannels {
            reply_tx,
            reply_rx,
            pending: Arc::new(Mutex::new(PendingHostEvents::default())),
        };
        let sink: Box<dyn Write + Send> = Box::new(io::sink());
        let writer = Arc::new(Mutex::new(sink));
        let worker = install_terminal_io_threads(
            &grid,
            Arc::new(Mutex::new(())),
            Box::new(io::empty()),
            writer,
            None,
            cache,
            host,
        )
        .expect("terminal io threads should spawn");

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = worker.join();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "writer thread should exit after its reply queue closes"
        );
    }

    #[test]
    fn cleanup_worker_enqueue_is_nonblocking_and_kills_separate_job_group() {
        assert_eq!(TERMINAL_CLEANUP_QUEUE_CAPACITY, 16);
        let (process, shell_pid, job_pid, _pid_path) = spawn_test_bash_with_background_job();
        let shell_stat = test_linux_stat(shell_pid).unwrap();
        let job_stat = test_linux_stat(job_pid).unwrap();
        let mut worker = TerminalCleanupWorker::new();
        let started = Instant::now();
        assert!(worker.try_enqueue(process, None).is_ok());
        let enqueue_elapsed = started.elapsed();
        assert!(
            enqueue_elapsed < Duration::from_millis(150),
            "UI-facing cleanup enqueue took {enqueue_elapsed:?}"
        );
        // Interactive bash ignores SIGTERM, so the shell goes only at SIGKILL.
        let kill_window = TERMINAL_SHUTDOWN_GRACE + Duration::from_secs(2);
        assert!(wait_for_process_gone(shell_pid, kill_window));
        assert!(wait_for_process_gone(job_pid, kill_window));
        worker.shutdown_and_join();
        println!(
            "async cleanup: enqueue={enqueue_elapsed:?} shell_pid={shell_pid} shell_pgid={} job_pid={job_pid} job_pgid={} sid={} shell_gone=true job_gone=true",
            shell_stat.process_group,
            job_stat.process_group,
            shell_stat.session_id,
        );
    }

    #[test]
    fn try_wait_reaps_direct_shell_without_running_blocking_session_cleanup() {
        let (mut process, shell_pid, job_pid, _pid_path) = spawn_test_bash_with_background_job();
        process.write_input(b"exit\r").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut max_try_wait = Duration::ZERO;
        loop {
            let started = Instant::now();
            let finished = process.try_wait().unwrap();
            max_try_wait = max_try_wait.max(started.elapsed());
            if finished {
                break;
            }
            assert!(Instant::now() < deadline, "shell did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            max_try_wait < Duration::from_millis(150),
            "try_wait blocked for {max_try_wait:?}"
        );
        assert!(!process_is_live(shell_pid));
        assert!(process_is_live(job_pid), "disowned same-SID job should outlive direct shell");

        let mut worker = TerminalCleanupWorker::new();
        assert!(worker.try_enqueue(process, None).is_ok());
        assert!(wait_for_process_gone(job_pid, Duration::from_secs(2)));
        worker.shutdown_and_join();
        println!("try_wait max={max_try_wait:?} shell_gone=true background_cleanup_job_gone=true");
    }

    #[test]
    fn final_shutdown_is_synchronous_session_cleanup_barrier() {
        let (mut process, shell_pid, job_pid, _pid_path) = spawn_test_bash_with_background_job();
        process.shutdown();
        assert!(!process_is_live(shell_pid));
        assert!(!process_is_live(job_pid));
        println!("final shutdown: shell_gone=true job_gone=true");
    }

    #[test]
    fn shutdown_waits_for_slow_sigterm_handler_in_separate_group_after_shell_exit() {
        let marker = unique_temp_path("shutdown-graceful.mark");
        let job = format!(
            "trap \"sleep 0.5; echo graceful > {}; exit 0\" TERM; echo $$ > PIDFILE; \
             while :; do sleep 0.05; done",
            marker.0.display()
        );
        let (mut process, shell_pid, job_pid, _pid_path) = spawn_test_session_with_job(&job);

        let started = Instant::now();
        process.shutdown();
        let elapsed = started.elapsed();

        assert!(!process_is_live(shell_pid));
        assert!(!process_is_live(job_pid));
        assert_eq!(
            std::fs::read_to_string(&marker.0).unwrap_or_default().trim(),
            "graceful",
            "job was killed before its SIGTERM handler finished"
        );
        assert!(
            elapsed < TERMINAL_SHUTDOWN_GRACE,
            "graceful session waited for SIGKILL escalation: {elapsed:?}"
        );
        println!("graceful shutdown: elapsed={elapsed:?} job handled SIGTERM");
    }

    #[test]
    fn concurrent_shutdown_kills_sigterm_ignoring_jobs_after_one_shared_grace() {
        let job = "trap \"\" TERM; echo $$ > PIDFILE; while :; do sleep 0.05; done";
        let (first, first_shell, first_job, _first_pid) = spawn_test_session_with_job(job);
        let (second, second_shell, second_job, _second_pid) = spawn_test_session_with_job(job);
        let mut processes = vec![first, second];

        let started = Instant::now();
        shutdown_terminal_processes(&mut processes);
        let elapsed = started.elapsed();

        for pid in [first_shell, first_job, second_shell, second_job] {
            assert!(!process_is_live(pid), "pid {pid} survived shutdown");
        }
        assert!(
            elapsed >= TERMINAL_SHUTDOWN_GRACE,
            "SIGTERM-ignoring job was killed before the grace period: {elapsed:?}"
        );
        assert!(
            elapsed < TERMINAL_SHUTDOWN_GRACE + Duration::from_millis(1500),
            "sessions were not shut down concurrently: {elapsed:?}"
        );
        drop(processes);
        println!("forced shutdown: two sessions elapsed={elapsed:?}");
    }

    #[test]
    fn terminal_presentation_waits_for_displayable_output_without_timeout_fallback() {
        let mut grid = test_term(24, 3);
        let mut sink = SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        let now = Instant::now();

        assert!(!grid.presentation_ready());
        advance_terminal_output_batch(
            &mut grid,
            &mut sink,
            &[output_chunk(b"\x1b[?25l\x1b[2J\r\n\t   \x1b[?25h")],
            now,
        );
        assert!(!grid.presentation_ready());

        advance_terminal_output_batch(
            &mut grid,
            &mut sink,
            &[output_chunk(b"\x1b[32muser@host> \x1b[0m")],
            now,
        );
        assert!(grid.presentation_ready());

        let source = include_str!("terminal_process.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        assert!(!production.contains("TERMINAL_PRESENTATION_FALLBACK"));
        assert!(production.contains(
            "let mut first = recv_output_chunk(&rx, &mut held_until, &mut flush_held_sync);"
        ));
    }

    #[test]
    fn terminal_stream_end_before_displayable_output_shows_explicit_state_once() {
        let mut grid = test_term(64, 3);
        let mut sink = SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        let now = Instant::now();
        advance_terminal_output_batch(
            &mut grid,
            &mut sink,
            &[output_chunk(b"\x1b[?25l\x1b[2J\r\n")],
            now,
        );
        assert!(!grid.presentation_ready());

        assert!(finish_terminal_output_stream(&mut grid, &mut sink, now).grid_changed);
        assert!(grid.presentation_ready());
        let text = (0..grid.total_rows())
            .filter_map(|row| grid.row(row))
            .flat_map(|line| line.iter().map(|cell| cell.ch()))
            .collect::<String>();
        assert!(text.contains("Ronsole terminal exited before producing output"));

        let rows_snapshot = |term: &Term| {
            (0..term.total_rows())
                .filter_map(|row| term.row(row).map(|line| line.to_vec()))
                .collect::<Vec<_>>()
        };
        let lines = rows_snapshot(&grid);
        assert!(!finish_terminal_output_stream(&mut grid, &mut sink, now).grid_changed);
        assert!(rows_snapshot(&grid) == lines);
    }

    #[test]
    fn sync_batch_does_not_bump_content_generation() {
        let mut grid = test_term(64, 3);
        let mut sink = SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        let before = grid.content_generation();

        advance_terminal_output_batch(
            &mut grid,
            &mut sink,
            &[output_chunk(b"\x1b[?2026hX")],
            Instant::now(),
        );

        assert_eq!(grid.content_generation(), before);
    }

    #[test]
    fn bell_run_coalesces_into_one_pending_flag() {
        let pending = Arc::new(Mutex::new(PendingHostEvents::default()));
        let wake = WakeHandle::new().expect("eventfd");
        let mut grid = test_term(64, 3);
        let mut sink = SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: Some(&pending),
            wake: Some(&wake),
        };
        advance_terminal_output_batch(
            &mut grid,
            &mut sink,
            &[output_chunk(b"\x07\x07\x07")],
            Instant::now(),
        );

        // The eventfd counter sums its writes, so one wakeup reads back as 1.
        assert_eq!(wake.drain().expect("drain"), 1);
        assert!(PendingHostEvents::take(&pending).bell);
        assert!(!PendingHostEvents::take(&pending).bell);
    }

    #[test]
    fn osc52_pending_value_replaced() {
        let pending = Arc::new(Mutex::new(PendingHostEvents::default()));
        let mut grid = test_term(64, 3);
        let mut sink = SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: Some(&pending),
            wake: None,
        };
        // "aGk=" is "hi" and "aGky" is "hi2" in base64; only the later value
        // survives until the main thread takes it.
        advance_terminal_output_batch(
            &mut grid,
            &mut sink,
            &[output_chunk(b"\x1b]52;c;aGk=\x07\x1b]52;c;aGky\x07")],
            Instant::now(),
        );

        let events = PendingHostEvents::take(&pending);
        assert_eq!(events.clipboard.as_deref(), Some("hi2"));
        assert!(PendingHostEvents::take(&pending).clipboard.is_none());
    }

    #[test]
    fn new_tab_starts_in_osc7_dir() {
        let home = PathBuf::from("/home/ronsole-test");
        let existing = std::env::temp_dir();
        assert_eq!(
            terminal_launch_directory(Some(existing.as_path()), &home),
            existing
        );
        let missing = PathBuf::from("/nonexistent/ronsole-osc7-dir");
        assert_eq!(
            terminal_launch_directory(Some(missing.as_path()), &home),
            home
        );
        assert_eq!(terminal_launch_directory(None, &home), home);
    }

    #[test]
    fn explicit_cli_directory_wins_over_inherited() {
        // The explicit folder is taken without an `is_dir` check, so a
        // missing path still wins over an existing inherited one.
        let explicit = PathBuf::from("/nonexistent/ronsole-cli-dir");
        let spec = TerminalLaunchSpec {
            working_directory: Some(explicit.clone()),
            inherited_directory: Some(std::env::temp_dir()),
            ..TerminalLaunchSpec::default()
        };

        let (_, cwd, _, _) =
            terminal_launch_command(spec, &TabToken::generate()).expect("launch command");

        assert_eq!(cwd, explicit);
    }

    #[test]
    fn finish_stream_always_marks_changed() {
        let mut grid = test_term(64, 3);
        let mut sink = SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        let now = Instant::now();
        // An open synchronized-output block holds the exit notice, so the
        // parser itself reports no grid change for it.
        advance_terminal_output_batch(
            &mut grid,
            &mut sink,
            &[output_chunk(b"\x1b[?2026hX")],
            now,
        );
        assert!(!grid.presentation_ready());
        let generation_before = grid.content_generation();

        let outcome = finish_terminal_output_stream(&mut grid, &mut sink, now);

        assert!(outcome.grid_changed);
        assert_ne!(grid.content_generation(), generation_before);
    }

    #[test]
    fn numbered_title_suffix_is_cached_and_survives_dynamic_titles() {
        let mut state = TerminalTitleState::new_numbered("fish".to_string(), 7);
        assert_eq!(resolved_title(&state), "fish (7)");
        state.observe_transition(Some("car-wash-api : htop".to_string()), false);
        assert_eq!(resolved_title(&state), "car-wash-api : htop (7)");
        state.observe_transition(Some("(reyan) server".to_string()), false);
        assert_eq!(resolved_title(&state), "(reyan) server (7)");
        assert_eq!(&*state.display_suffix, " (7)");
    }

    #[test]
    fn htop_title_is_not_duplicated() {
        let home = Path::new("/home/reyan");
        let cwd = Path::new("/home/reyan/projects/car-wash-api");
        let htop = process_snapshot("htop", "/home/reyan/projects/car-wash-api", &["htop"]);
        assert_eq!(
            terminal_title_for_snapshot(&htop, Some(cwd), Some(home), "fish"),
            "car-wash-api : htop"
        );
    }

    #[test]
    fn ssh_title_uses_user_and_host_from_process_arguments() {
        let home = Path::new("/home/reyan");
        let direct = process_snapshot(
            "ssh",
            "/home/reyan",
            &["ssh", "reyan@89.169.37.107"],
        );
        assert_eq!(
            terminal_title_for_snapshot(&direct, Some(home), Some(home), "fish"),
            "(reyan) 89.169.37.107"
        );

        let login_option = process_snapshot(
            "ssh",
            "/home/reyan",
            &["ssh", "-p", "2222", "-i", "/tmp/key", "-l", "reyan", "server"],
        );
        assert_eq!(
            terminal_title_for_snapshot(&login_option, Some(home), Some(home), "fish"),
            "(reyan) server"
        );
    }

    #[test]
    fn detected_process_and_ssh_titles_override_wrapper_osc() {
        let home = Path::new("/home/reyan");
        let cwd = Path::new("/home/reyan/projects/car-wash-api");
        let htop = process_snapshot("htop", "/home/reyan/projects/car-wash-api", &["htop"]);
        let mut state = TerminalTitleState::new("car-wash-api : fish".to_string());
        state.observe_transition(
            Some(terminal_title_for_snapshot(&htop, Some(cwd), Some(home), "fish")),
            false,
        );
        state.set_programmed("~/projects/car-wash-api: htop - htop".to_string());
        assert_eq!(resolved_title(&state), "car-wash-api : htop");

        let ssh = process_snapshot(
            "ssh",
            "/home/reyan/projects/car-wash-api",
            &["ssh", "reyan@89.169.37.107"],
        );
        state.observe_transition(
            Some(terminal_title_for_snapshot(&ssh, Some(cwd), Some(home), "fish")),
            false,
        );
        state.set_programmed("~/projects/car-wash-api: ssh_prod - ssh_prod".to_string());
        assert_eq!(resolved_title(&state), "(reyan) 89.169.37.107");
    }

    #[test]
    fn osc_title_reaches_shared_cache_through_sink() {
        let (mut grid, cache) = parser_grid();
        let now = Instant::now();
        let mut sink = SessionSink {
            reply_tx: None,
            title_cache: Some(&cache),
            pending: None,
            wake: None,
        };
        grid.advance(b"\x1b]0;~ : htop\x07", now, &mut sink);
        assert_eq!(resolved_title(&platform::lock_recover(&cache)), "~ : htop");

        grid.advance(b"\x1b]2;bin : sleep\x1b\\", now, &mut sink);
        assert_eq!(resolved_title(&platform::lock_recover(&cache)), "bin : sleep");
    }

    #[test]
    fn reply_from_sink_reaches_writer_while_writer_drains() {
        let mut grid = test_term(8, 2);
        let (reply_tx, reply_rx) = terminal_reply_channel();
        let mut sink = SessionSink {
            reply_tx: Some(&reply_tx),
            title_cache: None,
            pending: None,
            wake: None,
        };
        advance_terminal_output_batch(
            &mut grid,
            &mut sink,
            &[output_chunk(b"\x1b[c")],
            Instant::now(),
        );
        assert_eq!(reply_rx.try_recv().unwrap(), b"\x1b[?62c".to_vec());
    }

    #[test]
    fn foreground_transition_detects_child_and_exec_changes() {
        let wrapper = process_snapshot("fish", "/home/reyan", &["fish"]);
        let mut ssh = process_snapshot("ssh", "/home/reyan", &["ssh", "host"]);
        ssh.process_id = wrapper.process_id + 1;
        assert!(terminal_foreground_transitioned(
            Some(700),
            700,
            Some(&wrapper),
            Some(&ssh),
        ));

        let ssh_same_pid = process_snapshot("ssh", "/home/reyan", &["ssh", "host"]);
        assert!(terminal_foreground_transitioned(
            Some(700),
            700,
            Some(&wrapper),
            Some(&ssh_same_pid),
        ));
    }

    #[test]
    fn terminal_fallback_title_uses_home_marker_or_cwd_basename() {
        let home = Path::new("/home/reyan");
        assert_eq!(
            terminal_fallback_title_with_home(Some(home), Some(home), "fish"),
            "~ : fish"
        );
        assert_eq!(
            terminal_fallback_title_with_home(
                Some(Path::new("/home/reyan/bin")),
                Some(home),
                "bash",
            ),
            "bin : bash"
        );
    }
}
