//! Whether a tab whose foreground is Claude Code can close without the busy
//! confirmation. Classification is pure; the thin `/proc` and `~/.claude`
//! reading layer runs only when a close is requested. Any doubt keeps the tab
//! busy: a wrong "idle" answer kills a running turn.

use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::tab_input::{ProcGen, claude_generation};

/// `claude` subcommands that are clients of a daemon-hosted session: closing
/// them detaches without touching the session.
const CLIENT_SUBCOMMANDS: [&str; 2] = ["attach", "agents"];

/// Job states of `~/.claude/jobs/<jobId>/state.json` with no turn running.
const SETTLED_JOB_STATES: [&str; 3] = ["done", "failed", "stopped"];

/// Longest job id accepted as a directory name under `~/.claude/jobs`.
const MAX_JOB_ID_LEN: usize = 64;

/// Largest file the close rule reads: session, job state, and `/proc` files.
/// A longer file is an error, which keeps the tab busy.
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// Most `/proc/<pid>/task` entries the close rule walks before giving up.
const MAX_TASKS: usize = 256;

/// Most child processes the close rule examines before giving up.
const MAX_CHILDREN: usize = 256;

/// `comm` values of shells, which a `claude` process must not be running.
const SHELL_COMMS: [&[u8]; 5] = [b"sh", b"bash", b"zsh", b"fish", b"dash"];

/// Path fragment of the shell snapshots `claude` runs commands through.
const SHELL_SNAPSHOT_MARK: &[u8] = b"/.claude/shell-snapshots/";

/// Whether argv[1] of a NUL-separated `/proc/<pid>/cmdline` is one of
/// `CLIENT_SUBCOMMANDS`.
fn is_client_subcommand(cmdline: &[u8]) -> bool {
    cmdline
        .split(|byte| *byte == 0)
        .nth(1)
        .is_some_and(|arg| CLIENT_SUBCOMMANDS.iter().any(|name| name.as_bytes() == arg))
}

/// Whether a job `state.json` describes a settled job: `state` is one of
/// `SETTLED_JOB_STATES`, and `inFlight` is null or absent, or an object whose
/// `tasks` and `queued` are each absent or the integer 0. An explicit null
/// counter, a string, a fraction, a negative or a positive count is busy.
/// Malformed JSON or a non-object is not settled.
fn job_state_is_settled(state_json: &str) -> bool {
    let Ok(Value::Object(job)) = serde_json::from_str::<Value>(state_json) else {
        return false;
    };
    let settled = job
        .get("state")
        .and_then(Value::as_str)
        .is_some_and(|state| SETTLED_JOB_STATES.contains(&state));
    if !settled {
        return false;
    }
    match job.get("inFlight") {
        None | Some(Value::Null) => true,
        Some(Value::Object(in_flight)) => {
            is_absent_or_zero(in_flight.get("tasks")) && is_absent_or_zero(in_flight.get("queued"))
        }
        Some(_) => false,
    }
}

/// Whether a counter field is missing, or is exactly the integer 0.
fn is_absent_or_zero(counter: Option<&Value>) -> bool {
    match counter {
        None => true,
        Some(value) => value.as_u64() == Some(0),
    }
}

/// Whether the session file says the process is idle and not waiting on a
/// dialog, and, when it names a job, that the job is settled. Any missing,
/// mistyped or foreign-pid field makes the session busy.
fn session_allows_close(session_json: &str, process: ProcGen, job_state: Option<&str>) -> bool {
    let Ok(Value::Object(session)) = serde_json::from_str::<Value>(session_json) else {
        return false;
    };
    if session.get("pid").and_then(Value::as_u64) != Some(u64::from(process.pid)) {
        return false;
    }
    let started = session
        .get("procStart")
        .and_then(Value::as_str)
        .and_then(|text| text.parse::<u64>().ok());
    if started != Some(process.starttime) {
        return false;
    }
    if session.get("status").and_then(Value::as_str) != Some("idle") {
        return false;
    }
    if !matches!(session.get("waitingFor"), None | Some(Value::Null)) {
        return false;
    }
    match session.get("jobId") {
        None | Some(Value::Null) => true,
        Some(Value::String(job_id)) if is_job_id(job_id) => {
            job_state.is_some_and(job_state_is_settled)
        }
        Some(_) => false,
    }
}

/// One child of the `claude` process: the trimmed `comm` and the NUL-separated
/// `cmdline` of `/proc/<child>/`.
struct ChildProcess {
    comm: Vec<u8>,
    cmdline: Vec<u8>,
}

/// Whether `needle` occurs as a contiguous run inside `haystack`.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// Whether a child is a shell, or a shell started from a `~/.claude` snapshot.
fn is_shell_child(child: &ChildProcess) -> bool {
    SHELL_COMMS.contains(&child.comm.as_slice())
        || contains_bytes(&child.cmdline, SHELL_SNAPSHOT_MARK)
}

/// Everything the close rule reads about a non-client `claude` leader. A `None`
/// field is a read that failed, which keeps the tab busy.
#[derive(Default)]
struct CloseEvidence {
    session: Option<String>,
    job_state: Option<String>,
    children: Option<Vec<ChildProcess>>,
}

/// Close rule for a `claude` foreground process with NUL-separated `cmdline`
/// and the generation `process`. Client subcommands are safe outright. For
/// anything else, the session must be idle and, when it names a job, that job
/// must be settled, and no child may be a shell. An empty argv is never safe.
fn claude_close_is_safe(cmdline: &[u8], process: ProcGen, evidence: &CloseEvidence) -> bool {
    if cmdline.is_empty() {
        return false;
    }
    if is_client_subcommand(cmdline) {
        return true;
    }
    let (Some(session), Some(children)) = (evidence.session.as_deref(), evidence.children.as_deref())
    else {
        return false;
    };
    session_allows_close(session, process, evidence.job_state.as_deref())
        && !children.iter().any(is_shell_child)
}

fn is_job_id(job_id: &str) -> bool {
    !job_id.is_empty()
        && job_id.len() <= MAX_JOB_ID_LEN
        && job_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Job id named by a session file, if it is a well-formed one. Only used to
/// pick which job state to read; `claude_close_is_safe` checks the session's
/// pid and start time before trusting it.
fn job_id_of(session_json: &str) -> Option<String> {
    let Ok(Value::Object(session)) = serde_json::from_str::<Value>(session_json) else {
        return None;
    };
    let job_id = session.get("jobId")?.as_str()?;
    is_job_id(job_id).then(|| job_id.to_owned())
}

/// Reads a file of at most `MAX_FILE_BYTES`. Opened non-blocking so a FIFO
/// cannot stall the close; outside `/proc` only regular files pass. Errors
/// keep the tab busy.
fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    if !path.starts_with("/proc") && !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a regular file",
        ));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file exceeds the read cap",
        ));
    }
    Ok(bytes)
}

/// `read_capped` as UTF-8 text; `None` on any error or invalid UTF-8.
fn read_text(path: &Path) -> Option<String> {
    String::from_utf8(read_capped(path).ok()?).ok()
}

/// Session text of `process` and the text of the job state it names, read
/// from `<home>/.claude`. Each is `None` when missing, unreadable, or too big.
fn read_session_files(home: &Path, process: ProcGen) -> (Option<String>, Option<String>) {
    let claude = home.join(".claude");
    let session = read_text(&claude.join("sessions").join(format!("{}.json", process.pid)));
    let job_state = session
        .as_deref()
        .and_then(job_id_of)
        .and_then(|job_id| read_text(&claude.join("jobs").join(job_id).join("state.json")));
    (session, job_state)
}

/// `/proc/<pid>/<file>` as a path.
fn proc_path(pid: u32, file: &str) -> PathBuf {
    PathBuf::from(format!("/proc/{pid}/{file}"))
}

/// Drops one trailing `byte`, such as the newline of `comm` or the NUL that
/// ends `cmdline`.
fn without_trailing(mut bytes: Vec<u8>, byte: u8) -> Vec<u8> {
    if bytes.last() == Some(&byte) {
        bytes.pop();
    }
    bytes
}

/// Every child of the threads of `pid`, with its `comm` and `cmdline`. A child
/// that exits mid-read is skipped; any other failure, or more than
/// `MAX_TASKS` tasks or `MAX_CHILDREN` children, yields `None`.
fn read_children(pid: u32) -> Option<Vec<ChildProcess>> {
    let tasks = fs::read_dir(format!("/proc/{pid}/task")).ok()?;
    let mut child_pids = Vec::new();
    for (index, task) in tasks.enumerate() {
        if index >= MAX_TASKS {
            return None;
        }
        let task = task.ok()?;
        let listing = read_capped(&task.path().join("children")).ok()?;
        for token in listing.split(u8::is_ascii_whitespace) {
            if token.is_empty() {
                continue;
            }
            let child = std::str::from_utf8(token).ok()?.parse::<u32>().ok()?;
            child_pids.push(child);
            if child_pids.len() > MAX_CHILDREN {
                return None;
            }
        }
    }
    let mut children = Vec::with_capacity(child_pids.len());
    for child in child_pids {
        let comm = match read_capped(&proc_path(child, "comm")) {
            Ok(comm) => without_trailing(comm, b'\n'),
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        let cmdline = match read_capped(&proc_path(child, "cmdline")) {
            Ok(cmdline) => cmdline,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        };
        children.push(ChildProcess { comm, cmdline });
    }
    Some(children)
}

/// Reads everything the close rule needs about the `claude` leader `process`.
/// Reads `~/.claude` and `/proc`; a `None` field keeps the tab busy.
fn read_close_evidence(process: ProcGen) -> CloseEvidence {
    let (session, job_state) = match crate::platform::user_home_dir() {
        Some(home) => read_session_files(&home, process),
        None => (None, None),
    };
    CloseEvidence {
        session,
        job_state,
        children: read_children(process.pid),
    }
}

/// Whether the PTY foreground group `process_group` is led by a `claude`
/// process that `claude_close_is_safe` lets close without a prompt. Reads
/// `/proc` and `~/.claude`, so call it only when a close is requested.
pub(crate) fn claude_group_close_is_safe(process_group: u32) -> bool {
    let Some(process) = claude_generation(process_group) else {
        return false;
    };
    let Ok(cmdline) = read_capped(&proc_path(process_group, "cmdline")) else {
        return false;
    };
    let cmdline = without_trailing(cmdline, 0);
    let evidence = if is_client_subcommand(&cmdline) {
        CloseEvidence::default()
    } else {
        read_close_evidence(process)
    };
    // The leader must still be the same process after the files were read.
    claude_close_is_safe(&cmdline, process, &evidence)
        && claude_generation(process_group) == Some(process)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::atomic::{AtomicUsize, Ordering};

    const PROCESS: ProcGen = ProcGen {
        pid: 4242,
        starttime: 777,
    };
    const INTERACTIVE: &[u8] = b"/opt/claude-code/bin/claude\0--session-id\0abc\0--resume\0x";
    const DONE_IDLE: &str = r#"{"state":"done","tempo":"idle","inFlight":{"tasks":0,"queued":0,"kinds":[],"drainableMonitors":0}}"#;

    /// Session file text of `PROCESS` with `status` idle; `extra` adds fields.
    fn session(extra: &str) -> String {
        format!(r#"{{"pid":4242,"procStart":"777","status":"idle"{extra}}}"#)
    }

    fn child(comm: &str, cmdline: &str) -> ChildProcess {
        ChildProcess {
            comm: comm.as_bytes().to_vec(),
            cmdline: cmdline.as_bytes().to_vec(),
        }
    }

    /// Whether the close rule accepts `cmdline` with the given evidence files.
    fn close_with(
        cmdline: &[u8],
        session: &str,
        job_state: Option<&str>,
        children: Vec<ChildProcess>,
    ) -> bool {
        let evidence = CloseEvidence {
            session: Some(session.to_owned()),
            job_state: job_state.map(str::to_owned),
            children: Some(children),
        };
        claude_close_is_safe(cmdline, PROCESS, &evidence)
    }

    #[test]
    fn client_subcommands_close_without_a_job() {
        let none = CloseEvidence::default();
        assert!(claude_close_is_safe(b"/opt/claude-code/bin/claude\0attach\x005b95ef5b", PROCESS, &none));
        assert!(claude_close_is_safe(b"claude\0agents", PROCESS, &none));
        assert!(!claude_close_is_safe(b"claude\0attachx\x005b95ef5b", PROCESS, &none));
        assert!(!claude_close_is_safe(b"claude\0--verbose\0attach", PROCESS, &none));
        assert!(!claude_close_is_safe(b"claude\0\0attach", PROCESS, &none));
    }

    #[test]
    fn interactive_idle_without_children_closes() {
        assert!(close_with(INTERACTIVE, &session(""), None, Vec::new()));
        assert!(close_with(INTERACTIVE, &session(r#","kind":"interactive","jobId":null"#), None, Vec::new()));
    }

    #[test]
    fn non_idle_or_waiting_sessions_stay_busy() {
        let busy = [
            r#"{"pid":4242,"procStart":"777","status":"busy"}"#,
            r#"{"pid":4242,"procStart":"777","status":"waiting"}"#,
            r#"{"pid":4242,"procStart":"777","status":"idle","waitingFor":"dialog open"}"#,
            r#"{"pid":4242,"procStart":"777","status":"idle","waitingFor":1}"#,
            r#"{"pid":4242,"procStart":"777"}"#,
            r#"{"pid":4242,"procStart":"777","status":2}"#,
            r#"{"pid":4242,"procStart":"777","status":"shell"}"#,
        ];
        for session_text in busy {
            assert!(!close_with(INTERACTIVE, session_text, None, Vec::new()), "{session_text}");
        }
    }

    #[test]
    fn shell_children_keep_the_tab_busy() {
        let idle = session("");
        assert!(!close_with(INTERACTIVE, &idle, None, vec![child("zsh", "zsh")]));
        assert!(!close_with(
            INTERACTIVE,
            &idle,
            None,
            vec![child("node", "/usr/bin/node\0/.claude/shell-snapshots/snapshot-zsh-1.sh")]
        ));
        assert!(close_with(
            INTERACTIVE,
            &idle,
            None,
            vec![child("tg-watch", "tg-watch\0--daemon"), child("rust-analyzer", "rust-analyzer")]
        ));
    }

    #[test]
    fn interactive_or_resumed_claude_without_a_session_stays_busy() {
        let none = CloseEvidence::default();
        assert!(!claude_close_is_safe(INTERACTIVE, PROCESS, &none));
        assert!(!claude_close_is_safe(b"claude", PROCESS, &none));
    }

    #[test]
    fn empty_argv_is_never_safe() {
        let none = CloseEvidence::default();
        assert!(!claude_close_is_safe(b"", PROCESS, &none));
        assert!(!close_with(b"", &session(""), None, Vec::new()));
    }

    /// Session text of a background job `job_id`, with `status` as given.
    fn job_session(status: &str, job_id: &str) -> String {
        format!(
            r#"{{"pid":4242,"procStart":"777","kind":"bg","status":"{status}","jobId":"{job_id}"}}"#
        )
    }

    #[test]
    fn settled_jobs_close() {
        let idle = job_session("idle", "abcd1234");
        assert!(close_with(INTERACTIVE, &idle, Some(DONE_IDLE), Vec::new()));
        assert!(close_with(INTERACTIVE, &idle, Some(r#"{"state":"done","tempo":"idle"}"#), Vec::new()));
        assert!(close_with(INTERACTIVE, &idle, Some(r#"{"state":"done","inFlight":{}}"#), Vec::new()));
        assert!(close_with(
            INTERACTIVE,
            &idle,
            Some(r#"{"state":"failed","tempo":"idle","inFlight":null}"#),
            Vec::new()
        ));
        assert!(close_with(
            INTERACTIVE,
            &idle,
            Some(r#"{"state":"done","extra":[1,{"x":null}],"inFlight":{"tasks":0,"queued":0,"more":true}}"#),
            Vec::new()
        ));
    }

    #[test]
    fn running_or_waiting_jobs_stay_busy() {
        let idle = job_session("idle", "abcd1234");
        for state in [
            r#"{"state":"working","tempo":"active","inFlight":{"tasks":2}}"#,
            r#"{"state":"working","tempo":"active","inFlight":{"tasks":0}}"#,
            r#"{"state":"blocked","tempo":"blocked","inFlight":{"tasks":0}}"#,
            r#"{"state":"blocked","tempo":"blocked","inFlight":{"tasks":1}}"#,
            r#"{"state":"done","inFlight":{"tasks":1}}"#,
            r#"{"state":"failed","inFlight":{"tasks":3}}"#,
            r#"{"state":"done","inFlight":{"tasks":0,"queued":1}}"#,
        ] {
            assert!(!close_with(INTERACTIVE, &idle, Some(state), Vec::new()), "{state}");
        }
    }

    #[test]
    fn busy_session_with_a_settled_job_stays_busy() {
        let busy = job_session("busy", "abcd1234");
        assert!(!close_with(INTERACTIVE, &busy, Some(DONE_IDLE), Vec::new()));
    }

    #[test]
    fn malformed_job_state_stays_busy() {
        let idle = job_session("idle", "abcd1234");
        for state in [
            "",
            "{",
            "garbage",
            "null",
            "[]",
            r#"["done"]"#,
            r#""done""#,
            "{}",
            r#"{"state":null}"#,
            r#"{"state":1}"#,
            r#"{"state":"DONE"}"#,
            r#"{"state":"done","inFlight":0}"#,
            r#"{"state":"done","inFlight":[]}"#,
            r#"{"state":"done","inFlight":{"tasks":null}}"#,
            r#"{"state":"done","inFlight":{"tasks":"0"}}"#,
            r#"{"state":"done","inFlight":{"tasks":0.0}}"#,
            r#"{"state":"done","inFlight":{"tasks":0.5}}"#,
            r#"{"state":"done","inFlight":{"tasks":-1}}"#,
            r#"{"state":"done","inFlight":{"tasks":0,"queued":null}}"#,
            r#"{"state":"done","inFlight":{"tasks":0,"queued":"0"}}"#,
            r#"{"state":"done"} trailing"#,
        ] {
            assert!(!close_with(INTERACTIVE, &idle, Some(state), Vec::new()), "{state}");
        }
    }

    #[test]
    fn session_must_match_the_process_generation() {
        let idle = session("");
        assert!(session_allows_close(&idle, PROCESS, None));
        let reused = ProcGen {
            pid: 4242,
            starttime: 1,
        };
        assert!(!session_allows_close(&idle, reused, None));
        let other = ProcGen {
            pid: 4241,
            starttime: 777,
        };
        assert!(!session_allows_close(&idle, other, None));
        for broken in [
            "",
            "garbage",
            "[]",
            r#"{"pid":4242,"status":"idle"}"#,
            r#"{"pid":4242,"procStart":777,"status":"idle"}"#,
            r#"{"pid":"4242","procStart":"777","status":"idle"}"#,
            r#"{"pid":4242,"procStart":"777","status":"idle","jobId":7}"#,
            r#"{"pid":4242,"procStart":"777","status":"idle","jobId":""}"#,
            r#"{"pid":4242,"procStart":"777","status":"idle","jobId":"../x"}"#,
        ] {
            assert!(!close_with(INTERACTIVE, broken, Some(DONE_IDLE), Vec::new()), "{broken}");
        }
        assert!(!close_with(
            INTERACTIVE,
            r#"{"pid":31268,"procStart":"777","status":"idle"}"#,
            None,
            Vec::new()
        ));
    }

    /// Temp home directory removed on drop, so a failing test leaves nothing.
    struct TempHome(PathBuf);

    impl TempHome {
        fn new(tag: &str) -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "ronsole-claude-close-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn claude(&self) -> PathBuf {
            let claude = self.0.join(".claude");
            fs::create_dir_all(claude.join("sessions")).unwrap();
            fs::create_dir_all(claude.join("jobs")).unwrap();
            claude
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn idle_bg_job_files_under_home_close() {
        let home = TempHome::new("job");
        let claude = home.claude();
        fs::create_dir_all(claude.join("jobs").join("abcd1234")).unwrap();
        fs::write(claude.join("sessions").join("4242.json"), job_session("idle", "abcd1234")).unwrap();
        fs::write(claude.join("jobs").join("abcd1234").join("state.json"), DONE_IDLE).unwrap();
        let (session_text, state_text) = read_session_files(&home.0, PROCESS);
        assert!(session_text.is_some());
        assert_eq!(state_text.as_deref(), Some(DONE_IDLE));
        assert!(close_with(
            INTERACTIVE,
            session_text.as_deref().unwrap_or_default(),
            state_text.as_deref(),
            Vec::new()
        ));
    }

    #[test]
    fn unreadable_or_oversized_files_under_home_stay_busy() {
        let home = TempHome::new("bad");
        let claude = home.claude();
        let session_path = claude.join("sessions").join("4242.json");
        let state_dir = claude.join("jobs").join("abcd1234").join("state.json");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(&session_path, job_session("idle", "abcd1234")).unwrap();
        // state.json is a directory: the job state is unreadable.
        let (session_text, state_text) = read_session_files(&home.0, PROCESS);
        assert_eq!(state_text, None);
        assert!(!close_with(
            INTERACTIVE,
            session_text.as_deref().unwrap_or_default(),
            state_text.as_deref(),
            Vec::new()
        ));
        // A state file over the cap is an error too.
        fs::remove_dir_all(&state_dir).unwrap();
        fs::write(&state_dir, vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(read_session_files(&home.0, PROCESS).1, None);
        // A session file over the cap is an error.
        fs::write(&session_path, vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        assert_eq!(read_session_files(&home.0, PROCESS).0, None);
        // Invalid UTF-8 is busy.
        fs::write(&session_path, b"\xff\xfe{}").unwrap();
        assert_eq!(read_session_files(&home.0, PROCESS).0, None);
    }

    #[test]
    fn fifo_session_file_stays_busy_without_blocking() {
        use std::os::unix::ffi::OsStrExt;
        let home = TempHome::new("fifo");
        let claude = home.claude();
        let fifo = claude.join("sessions").join("4242.json");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `c_path` is a valid NUL-terminated path inside the temp home.
        let made = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(made, 0);
        let (session_text, _) = read_session_files(&home.0, PROCESS);
        assert_eq!(session_text, None);
    }

    #[test]
    fn session_file_of_another_pid_under_the_right_name_stays_busy() {
        let home = TempHome::new("foreign");
        let claude = home.claude();
        fs::write(
            claude.join("sessions").join("4242.json"),
            r#"{"pid":9999,"procStart":"777","status":"idle"}"#,
        )
        .unwrap();
        let (session_text, _) = read_session_files(&home.0, PROCESS);
        assert!(session_text.is_some());
        assert!(!close_with(
            INTERACTIVE,
            session_text.as_deref().unwrap_or_default(),
            None,
            Vec::new()
        ));
    }

    #[test]
    fn non_claude_group_is_never_safe() {
        assert!(!claude_group_close_is_safe(std::process::id()));
        assert!(!claude_group_close_is_safe(0));
    }
}
