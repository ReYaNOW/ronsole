//! Whether a tab whose foreground is Claude Code can close without the busy
//! confirmation. Classification is pure; the thin `/proc` and `~/.claude`
//! reading layer runs only when a close is requested. Any doubt keeps the tab
//! busy: a wrong "idle" answer kills a running turn.

use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::tab_input::{ProcGen, claude_generation};

/// `claude` subcommands that are clients of a daemon-hosted session: closing
/// them detaches without touching the session.
const CLIENT_SUBCOMMANDS: [&str; 2] = ["attach", "agents"];

/// Job states of `~/.claude/jobs/<jobId>/state.json` with no turn running.
const SETTLED_JOB_STATES: [&str; 3] = ["done", "failed", "stopped"];

/// Longest job id accepted as a directory name under `~/.claude/jobs`.
const MAX_JOB_ID_LEN: usize = 64;

/// Whether argv[1] of a NUL-separated `/proc/<pid>/cmdline` is one of
/// `CLIENT_SUBCOMMANDS`.
fn is_client_subcommand(cmdline: &[u8]) -> bool {
    cmdline
        .split(|byte| *byte == 0)
        .nth(1)
        .is_some_and(|arg| CLIENT_SUBCOMMANDS.iter().any(|name| name.as_bytes() == arg))
}

/// Whether a job `state.json` describes a settled job: `state` is one of
/// `SETTLED_JOB_STATES` and `inFlight.tasks` is 0, null or absent. Malformed
/// JSON, a non-object, or any unexpected field type is not settled.
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
        Some(Value::Object(in_flight)) => match in_flight.get("tasks") {
            None | Some(Value::Null) => true,
            Some(tasks) => tasks.as_u64() == Some(0),
        },
        Some(_) => false,
    }
}

/// Close rule for a `claude` foreground process with NUL-separated `cmdline`:
/// safe when argv[1] is a client subcommand, or when `job_state` (the content
/// of the session's job `state.json`, if found) is settled. An empty argv is
/// never safe.
pub(crate) fn claude_close_is_safe(cmdline: &[u8], job_state: Option<&str>) -> bool {
    if cmdline.is_empty() {
        return false;
    }
    is_client_subcommand(cmdline) || job_state.is_some_and(job_state_is_settled)
}

fn is_job_id(job_id: &str) -> bool {
    !job_id.is_empty()
        && job_id.len() <= MAX_JOB_ID_LEN
        && job_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Job id from a `~/.claude/sessions/<pid>.json` written by the process
/// generation `process`: its `pid` must match and its string `procStart` must
/// equal the start time (`/proc/<pid>/stat` field 22), so a file left by an
/// earlier process with a reused pid yields `None`.
fn session_job_id(session_json: &str, process: ProcGen) -> Option<String> {
    let Ok(Value::Object(session)) = serde_json::from_str::<Value>(session_json) else {
        return None;
    };
    if session.get("pid").and_then(Value::as_u64) != Some(u64::from(process.pid)) {
        return None;
    }
    let started = session.get("procStart")?.as_str()?.parse::<u64>().ok()?;
    if started != process.starttime {
        return None;
    }
    let job_id = session.get("jobId")?.as_str()?;
    is_job_id(job_id).then(|| job_id.to_owned())
}

/// Content of the job `state.json` of the session run by `process`, found
/// through `<home>/.claude/sessions/<pid>.json`; `None` on any missing or
/// unreadable file or a session file of another process.
fn read_job_state(home: &Path, process: ProcGen) -> Option<String> {
    let claude = home.join(".claude");
    let session = fs::read_to_string(
        claude
            .join("sessions")
            .join(format!("{}.json", process.pid)),
    )
    .ok()?;
    let job_id = session_job_id(&session, process)?;
    fs::read_to_string(claude.join("jobs").join(job_id).join("state.json")).ok()
}

/// Whether the PTY foreground group `process_group` is led by a `claude`
/// process that `claude_close_is_safe` lets close without a prompt. Reads
/// `/proc` and `~/.claude`, so call it only when a close is requested.
pub(crate) fn claude_group_close_is_safe(process_group: u32) -> bool {
    let Some(process) = claude_generation(process_group) else {
        return false;
    };
    let Ok(mut cmdline) = fs::read(format!("/proc/{process_group}/cmdline")) else {
        return false;
    };
    if cmdline.last() == Some(&0) {
        cmdline.pop();
    }
    let job_state = if is_client_subcommand(&cmdline) {
        None
    } else {
        crate::platform::user_home_dir().and_then(|home| read_job_state(&home, process))
    };
    // The leader must still be the same process after the files were read.
    claude_close_is_safe(&cmdline, job_state.as_deref())
        && claude_generation(process_group) == Some(process)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DONE_IDLE: &str = r#"{"state":"done","tempo":"idle","inFlight":{"tasks":0,"queued":0,"kinds":[],"drainableMonitors":0}}"#;
    const INTERACTIVE: &[u8] = b"/opt/claude-code/bin/claude\0--session-id\0abc\0--resume\0x";

    #[test]
    fn client_subcommands_close_without_a_job() {
        assert!(claude_close_is_safe(b"/opt/claude-code/bin/claude\0attach\x005b95ef5b", None));
        assert!(claude_close_is_safe(b"claude\0agents", None));
        assert!(!claude_close_is_safe(b"claude\0attachx\x005b95ef5b", None));
        assert!(!claude_close_is_safe(b"claude\0--verbose\0attach", None));
        assert!(!claude_close_is_safe(b"claude\0\0attach", None));
    }

    #[test]
    fn interactive_or_resumed_claude_without_a_job_stays_busy() {
        assert!(!claude_close_is_safe(INTERACTIVE, None));
        assert!(!claude_close_is_safe(b"claude", None));
    }

    #[test]
    fn empty_argv_is_never_safe() {
        assert!(!claude_close_is_safe(b"", None));
        assert!(!claude_close_is_safe(b"", Some(DONE_IDLE)));
    }

    #[test]
    fn settled_jobs_close() {
        assert!(claude_close_is_safe(INTERACTIVE, Some(DONE_IDLE)));
        assert!(claude_close_is_safe(INTERACTIVE, Some(r#"{"state":"done","tempo":"idle"}"#)));
        assert!(claude_close_is_safe(INTERACTIVE, Some(r#"{"state":"done","inFlight":{}}"#)));
        assert!(claude_close_is_safe(
            INTERACTIVE,
            Some(r#"{"state":"failed","tempo":"idle","inFlight":null}"#)
        ));
        assert!(claude_close_is_safe(
            INTERACTIVE,
            Some(r#"{"state":"stopped","inFlight":{"tasks":null}}"#)
        ));
        assert!(claude_close_is_safe(
            INTERACTIVE,
            Some(r#"{"state":"done","extra":[1,{"x":null}],"inFlight":{"tasks":0,"more":true}}"#)
        ));
    }

    #[test]
    fn running_or_waiting_jobs_stay_busy() {
        for state in [
            r#"{"state":"working","tempo":"active","inFlight":{"tasks":2}}"#,
            r#"{"state":"working","tempo":"active","inFlight":{"tasks":0}}"#,
            r#"{"state":"blocked","tempo":"blocked","inFlight":{"tasks":0}}"#,
            r#"{"state":"blocked","tempo":"blocked","inFlight":{"tasks":1}}"#,
            r#"{"state":"done","inFlight":{"tasks":1}}"#,
            r#"{"state":"failed","inFlight":{"tasks":3}}"#,
        ] {
            assert!(!claude_close_is_safe(INTERACTIVE, Some(state)), "{state}");
        }
    }

    #[test]
    fn malformed_job_state_stays_busy() {
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
            r#"{"state":"done","inFlight":{"tasks":"0"}}"#,
            r#"{"state":"done","inFlight":{"tasks":0.0}}"#,
            r#"{"state":"done","inFlight":{"tasks":-1}}"#,
            r#"{"state":"done"} trailing"#,
        ] {
            assert!(!claude_close_is_safe(INTERACTIVE, Some(state)), "{state}");
        }
    }

    #[test]
    fn session_job_id_requires_the_same_process_generation() {
        let process = ProcGen {
            pid: 31268,
            starttime: 421142,
        };
        let session = r#"{"pid":31268,"sessionId":"0778375e-1d2e","procStart":"421142","kind":"bg","jobId":"22493cec","status":"busy"}"#;
        assert_eq!(session_job_id(session, process).as_deref(), Some("22493cec"));
        let reused = ProcGen {
            pid: 31268,
            starttime: 1,
        };
        assert_eq!(session_job_id(session, reused), None);
        let other = ProcGen {
            pid: 31267,
            starttime: 421142,
        };
        assert_eq!(session_job_id(session, other), None);
        for broken in [
            "",
            "garbage",
            "[]",
            r#"{"pid":31268,"procStart":"421142"}"#,
            r#"{"pid":31268,"procStart":421142,"jobId":"22493cec"}"#,
            r#"{"pid":"31268","procStart":"421142","jobId":"22493cec"}"#,
            r#"{"pid":31268,"procStart":"421142","jobId":""}"#,
            r#"{"pid":31268,"procStart":"421142","jobId":"../x"}"#,
            r#"{"pid":31268,"procStart":"421142","jobId":7}"#,
        ] {
            assert_eq!(session_job_id(broken, process), None, "{broken}");
        }
    }

    #[test]
    fn read_job_state_follows_the_session_file_of_the_process() {
        let home = std::env::temp_dir().join(format!("ronsole-claude-close-{}", std::process::id()));
        let process = ProcGen {
            pid: 4242,
            starttime: 777,
        };
        let sessions = home.join(".claude").join("sessions");
        let job = home.join(".claude").join("jobs").join("abcd1234");
        fs::create_dir_all(&sessions).unwrap();
        fs::create_dir_all(&job).unwrap();
        assert_eq!(read_job_state(&home, process), None);
        fs::write(
            sessions.join("4242.json"),
            r#"{"pid":4242,"procStart":"777","jobId":"abcd1234"}"#,
        )
        .unwrap();
        assert_eq!(read_job_state(&home, process), None);
        fs::write(job.join("state.json"), DONE_IDLE).unwrap();
        assert_eq!(read_job_state(&home, process).as_deref(), Some(DONE_IDLE));
        let reused = ProcGen {
            pid: 4242,
            starttime: 778,
        };
        assert_eq!(read_job_state(&home, reused), None);
        fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn non_claude_group_is_never_safe() {
        assert!(!claude_group_close_is_safe(std::process::id()));
        assert!(!claude_group_close_is_safe(0));
    }
}
