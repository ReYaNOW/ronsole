use crate::platform::{self, ProcessTree};
use crate::terminal::TermGrid;
use crate::wake::WakeHandle;
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc::{SyncSender, TrySendError},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use vte::Parser;

const INITIAL_COLS: u16 = 200;
const INITIAL_ROWS: u16 = 60;
const TERMINAL_SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
const TERMINAL_TITLE_REFRESH_INTERVAL: Duration = Duration::from_millis(250);
const TERMINAL_OUTPUT_BATCH_IDLE: Duration = Duration::from_millis(8);
const TERMINAL_OUTPUT_BATCH_MAX: Duration = Duration::from_millis(32);
const TERMINAL_OUTPUT_BATCH_MAX_CHUNKS: usize = 32;
const TERMINAL_OUTPUT_QUEUE_CAPACITY: usize = 16;
pub(crate) const TERMINAL_REPLY_QUEUE_CAPACITY: usize = 32;
pub(crate) const TERMINAL_CLEANUP_QUEUE_CAPACITY: usize = 16;
const TERMINAL_READ_BUFFER_SIZE: usize = 65_536;
const TERMINAL_EXIT_BEFORE_OUTPUT: &[u8] = b"Ronsole terminal exited before producing output\r\n";
pub(crate) const TERMINAL_TITLE_MAX_BYTES: usize = 256;

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
}

pub(crate) type TerminalTitleCache = Arc<Mutex<TerminalTitleState>>;

impl TerminalTitleState {
    pub(crate) fn new(fallback: String) -> Self {
        Self {
            fallback,
            detected: None,
            programmed: None,
            display_suffix: Box::from(""),
            generation: 0,
            observation_serial: 0,
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
    tree: ProcessTree,
    title_stop_tx: Option<std::sync::mpsc::Sender<()>>,
    title_worker: Option<JoinHandle<()>>,
    finished: bool,
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
            .spawn(move || {
                while let Ok(mut job) = receiver.recv() {
                    job.process.shutdown();
                    if let Some(wake) = job.wake {
                        wake.wake();
                    }
                }
            })
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

impl Drop for TerminalCleanupWorker {
    fn drop(&mut self) {
        self.shutdown_and_join();
    }
}

impl TerminalProcess {
    pub(crate) fn spawn(
        grid: Arc<Mutex<TermGrid>>,
        title_cache: TerminalTitleCache,
        wake: Option<WakeHandle>,
    ) -> io::Result<Self> {
        let cwd = terminal_working_directory()?;
        let shell = resolve_terminal_shell()?;
        let fallback = terminal_fallback_title(Some(&cwd), &shell.title);
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

        let mut command = CommandBuilder::new(&shell.executable);
        command.cwd(cwd.as_os_str());
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");

        let mut child = pair.slave.spawn_command(command).map_err(|error| {
            io::Error::other(format!("failed to spawn terminal shell: {error}"))
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
            title_cache,
            shell.title,
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

        if let Err(error) = install_terminal_io_threads(&grid, reader, writer.clone(), wake) {
            let _ = title_stop_tx.send(());
            platform::reap_unit_thread(title_worker);
            let _ = tree.terminate_forcefully();
            let _ = child.kill();
            let _ = child.wait();
            tree.finish_after_owner_exit();
            return Err(error);
        }

        Ok(Self {
            writer,
            master_pty,
            child,
            tree,
            title_stop_tx: Some(title_stop_tx),
            title_worker: Some(title_worker),
            finished: false,
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

    pub(crate) fn shutdown(&mut self) {
        self.stop_title_refresh();
        if self.finished {
            self.tree.finish_after_owner_exit();
            return;
        }

        let _ = self.tree.terminate_gracefully();
        let deadline = Instant::now() + TERMINAL_SHUTDOWN_GRACE;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.finished = true;
                    self.tree.finish_after_owner_exit();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(_) => break,
            }
        }

        let _ = self.tree.terminate_forcefully();
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.finished = true;
        self.tree.finish_after_owner_exit();
    }
}

impl Drop for TerminalProcess {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub(crate) fn terminal_programmed_title(parts: &[&[u8]]) -> Option<String> {
    let capacity = parts
        .iter()
        .fold(parts.len().saturating_sub(1), |total, part| {
            total.saturating_add(part.len())
        })
        .min(TERMINAL_TITLE_MAX_BYTES);
    let mut title = String::with_capacity(capacity);

    'parts: for (index, part) in parts.iter().enumerate() {
        if index != 0 {
            if title.len() == TERMINAL_TITLE_MAX_BYTES {
                break;
            }
            title.push(';');
        }
        let part = std::str::from_utf8(part).ok()?;
        for ch in part.chars() {
            if ch.is_control() {
                continue;
            }
            if title.len() + ch.len_utf8() > TERMINAL_TITLE_MAX_BYTES {
                break 'parts;
            }
            title.push(ch);
        }
    }

    let first_non_whitespace = title
        .char_indices()
        .find_map(|(index, ch)| (!ch.is_whitespace()).then_some(index))?;
    let trimmed_len = title.trim_end().len();
    title.truncate(trimmed_len);
    if first_non_whitespace != 0 {
        title.drain(..first_non_whitespace);
    }
    (!title.is_empty()).then_some(title)
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

fn refresh_terminal_title_cache(
    master_pty: &Arc<Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    title_cache: &TerminalTitleCache,
    shell_title: &str,
    initial_cwd: Option<&Path>,
    home: Option<&Path>,
    last_process_group: &mut Option<u32>,
    snapshot: &mut Option<platform::ProcessSnapshot>,
) -> bool {
    let process_group = platform::lock_recover(master_pty)
        .process_group_leader()
        .and_then(|pid| u32::try_from(pid).ok());
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
            if refresh_terminal_title_cache(
                &master_pty,
                &title_cache,
                &shell_title,
                Some(&initial_cwd),
                home.as_deref(),
                &mut last_process_group,
                &mut snapshot,
            ) && let Some(wake) = wake.as_ref()
            {
                wake.wake();
            }

            match stop_rx.recv_timeout(TERMINAL_TITLE_REFRESH_INTERVAL) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    })?;
    Ok((stop_tx, worker))
}

fn advance_terminal_output_batch(
    parser: &mut Parser,
    grid: &mut TermGrid,
    chunks: &[OutputChunk],
) {
    for chunk in chunks {
        parser.advance(grid, &chunk.bytes[..chunk.len]);
    }
    grid.content_generation = grid.content_generation.wrapping_add(1);
    grid.dirty = true;
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

fn finish_terminal_output_stream(parser: &mut Parser, grid: &mut TermGrid) -> bool {
    if grid.presentation_ready {
        return false;
    }

    parser.advance(grid, TERMINAL_EXIT_BEFORE_OUTPUT);
    grid.content_generation = grid.content_generation.wrapping_add(1);
    true
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

fn terminal_reply_channel() -> (
    std::sync::mpsc::SyncSender<Vec<u8>>,
    std::sync::mpsc::Receiver<Vec<u8>>,
) {
    std::sync::mpsc::sync_channel(TERMINAL_REPLY_QUEUE_CAPACITY)
}

fn install_terminal_io_threads(
    grid: &Arc<Mutex<TermGrid>>,
    reader: Box<dyn Read + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    wake: Option<WakeHandle>,
) -> io::Result<()> {
    let (reply_tx, reply_rx) = terminal_reply_channel();
    let (tx, rx) = std::sync::mpsc::sync_channel::<OutputChunk>(TERMINAL_OUTPUT_QUEUE_CAPACITY);
    let (recycle_tx, recycle_rx) =
        std::sync::mpsc::sync_channel::<Vec<u8>>(TERMINAL_OUTPUT_QUEUE_CAPACITY);
    let reader_finished = Arc::new(AtomicBool::new(false));

    let parser_grid = grid.clone();
    let parser_reader_finished = reader_finished.clone();
    platform::spawn_named("ronsole-terminal-parser", move || {
        let mut parser = Parser::new();
        let wake_main_loop = || {
            if let Some(wake) = wake.as_ref() {
                wake.wake();
            }
        };
        let mut batch = Vec::with_capacity(8);
        let mut first = rx.recv().ok();

        while let Some(first_chunk) = first {
            batch.push(first_chunk);
            let started = Instant::now();
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

                {
                    let mut grid = platform::lock_recover(&parser_grid);
                    advance_terminal_output_batch(&mut parser, &mut grid, &batch);
                }
                recycle_output_chunks(&mut batch, &recycle_tx);

                match action {
                    TerminalOutputBatchAction::FlushAndContinue => {
                        let Some(timeout) =
                            terminal_output_batch_receive_timeout(started.elapsed())
                        else {
                            wake_main_loop();
                            break;
                        };
                        match rx.recv_timeout(timeout) {
                            Ok(next) => batch.push(next),
                            Err(_) => {
                                wake_main_loop();
                                break;
                            }
                        }
                    }
                    TerminalOutputBatchAction::Collect
                    | TerminalOutputBatchAction::FlushAndRedraw => {
                        wake_main_loop();
                        break;
                    }
                }
            }
            first = rx.recv().ok();
        }

        if parser_reader_finished.load(Ordering::Acquire) {
            let mut grid = platform::lock_recover(&parser_grid);
            if finish_terminal_output_stream(&mut parser, &mut grid) {
                drop(grid);
                wake_main_loop();
            }
        }
    })
    .map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to spawn terminal parser: {error}"),
        )
    })?;

    platform::spawn_named("ronsole-terminal-writer", move || {
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

    platform::lock_recover(grid).reply_tx = Some(reply_tx);
    Ok(())
}

pub(crate) fn resolve_terminal_shell() -> io::Result<TerminalShellSpec> {
    resolve_terminal_shell_with(std::env::var_os("SHELL"), platform::resolve_executable)
}

fn resolve_terminal_shell_with(
    shell_env: Option<OsString>,
    mut resolve: impl FnMut(&OsStr) -> Option<PathBuf>,
) -> io::Result<TerminalShellSpec> {
    for candidate in terminal_shell_candidates_with(shell_env) {
        if let Some(executable) = resolve(&candidate) {
            let title = terminal_shell_title(&executable);
            return Ok(TerminalShellSpec { executable, title });
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no supported terminal shell was found",
    ))
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

    fn spawn_test_bash_with_background_job() -> (TerminalProcess, u32, u32, TempPidPath) {
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
        let writer = Arc::new(Mutex::new(pair.master.take_writer().unwrap()));
        let master_pty = Arc::new(Mutex::new(pair.master));
        let process = TerminalProcess {
            writer,
            master_pty,
            child,
            tree,
            title_stop_tx: None,
            title_worker: None,
            finished: false,
        };

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

    fn output_chunk(bytes: &[u8]) -> OutputChunk {
        let mut buffer = vec![0; TERMINAL_READ_BUFFER_SIZE];
        buffer[..bytes.len()].copy_from_slice(bytes);
        OutputChunk {
            bytes: buffer,
            len: bytes.len(),
        }
    }

    fn parser_grid() -> (TermGrid, TerminalTitleCache) {
        let cache = Arc::new(Mutex::new(TerminalTitleState::new("fallback".to_string())));
        (
            TermGrid::new_with_title_cache(8, 2, cache.clone()),
            cache,
        )
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
        let fish = resolve_terminal_shell_with(Some(OsString::from("/usr/bin/fish")), |candidate| {
            (candidate == OsStr::new("/usr/bin/fish")).then(|| PathBuf::from("/usr/bin/fish"))
        })
        .unwrap();
        assert_eq!(fish.executable, PathBuf::from("/usr/bin/fish"));
        assert_eq!(fish.title, "fish");

        let bash = resolve_terminal_shell_with(Some(OsString::from("/missing/shell")), |candidate| {
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
    fn terminal_environment_advertises_implemented_truecolor() {
        let source = include_str!("terminal_process.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        assert!(production.contains("command.env(\"TERM\", \"xterm-256color\")"));
        assert!(production.contains("command.env(\"COLORTERM\", \"truecolor\")"));
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
        assert!(wait_for_process_gone(shell_pid, Duration::from_secs(2)));
        assert!(wait_for_process_gone(job_pid, Duration::from_secs(2)));
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
    fn terminal_presentation_waits_for_displayable_output_without_timeout_fallback() {
        let mut grid = TermGrid::new(24, 3);
        let mut parser = Parser::new();

        assert!(!grid.presentation_ready);
        advance_terminal_output_batch(
            &mut parser,
            &mut grid,
            &[output_chunk(b"\x1b[?25l\x1b[2J\r\n\t   \x1b[?25h")],
        );
        assert!(!grid.presentation_ready);

        advance_terminal_output_batch(
            &mut parser,
            &mut grid,
            &[output_chunk(b"\x1b[32muser@host> \x1b[0m")],
        );
        assert!(grid.presentation_ready);

        let source = include_str!("terminal_process.rs");
        let production = source.split("\n#[cfg(test)]").next().unwrap_or(source);
        assert!(!production.contains("TERMINAL_PRESENTATION_FALLBACK"));
        assert!(production.contains("let mut first = rx.recv().ok();"));
    }

    #[test]
    fn terminal_stream_end_before_displayable_output_shows_explicit_state_once() {
        let mut grid = TermGrid::new(64, 3);
        let mut parser = Parser::new();
        advance_terminal_output_batch(
            &mut parser,
            &mut grid,
            &[output_chunk(b"\x1b[?25l\x1b[2J\r\n")],
        );
        assert!(!grid.presentation_ready);

        assert!(finish_terminal_output_stream(&mut parser, &mut grid));
        assert!(grid.presentation_ready);
        let text = grid
            .lines
            .iter()
            .flat_map(|line| line.iter().map(|cell| cell.c))
            .collect::<String>();
        assert!(text.contains("Ronsole terminal exited before producing output"));

        let lines = grid.lines.clone();
        assert!(!finish_terminal_output_stream(&mut parser, &mut grid));
        assert!(grid.lines == lines);
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
    fn osc_titles_are_control_safe_bounded_and_shared_with_grid() {
        let (mut grid, cache) = parser_grid();
        let mut parser = Parser::new();
        parser.advance(&mut grid, b"\x1b]0;~ : htop\x07");
        assert_eq!(resolved_title(&platform::lock_recover(&cache)), "~ : htop");

        parser.advance(&mut grid, b"\x1b]2;bin : sleep\x1b\\");
        assert_eq!(resolved_title(&platform::lock_recover(&cache)), "bin : sleep");

        assert_eq!(terminal_programmed_title(&[b"\xff"]), None);
        assert_eq!(
            terminal_programmed_title(&[b"  safe\n\0title  "]).as_deref(),
            Some("safetitle")
        );
        let huge = "x".repeat(TERMINAL_TITLE_MAX_BYTES * 8);
        let sequence = format!("\x1b]2;{huge}\x07");
        parser.advance(&mut grid, sequence.as_bytes());
        assert_eq!(
            resolved_title(&platform::lock_recover(&cache)).len(),
            TERMINAL_TITLE_MAX_BYTES
        );
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
