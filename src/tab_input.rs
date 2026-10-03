//! Parsing of the Claude Code input box and `/proc`-based process checks used
//! by the `TAB_INPUT` delivery path. Pure logic: no terminal or socket state.

use std::fs;

/// State of the Claude Code input box as seen in the visible screen rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputBox {
    /// Box found and its only line is the bare prompt `❯`.
    EmptyBusy,
    /// Box found and it holds any text (including the idle placeholder).
    NotEmpty,
    /// No complete box, an empty box, or a first line that is not the prompt.
    Unrecognized,
}

/// Identity of one process generation: pid plus start time in clock ticks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcGen {
    pub(crate) pid: u32,
    pub(crate) starttime: u64,
}

const PROMPT: char = '❯';
const RULE: char = '─';
const MAX_WALK_STEPS: usize = 64;

/// A rule row has only `─` besides whitespace, and at least half of the
/// widest row's width (rows arrive right-trimmed, so the widest row stands in
/// for the window width).
fn is_rule(row: &str, width: usize) -> bool {
    let mut count = 0usize;
    for ch in row.chars() {
        if ch == RULE {
            count += 1;
        } else if !ch.is_whitespace() {
            return false;
        }
    }
    count > 0 && count * 2 >= width
}

/// Classify the input box from the tail rows of the visible screen
/// (`Terminal::screen_tail_text` output).
pub(crate) fn classify_input_box(rows: &[String]) -> InputBox {
    let width = rows.iter().map(|row| row.chars().count()).max().unwrap_or(0);
    let mut rules = rows
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, row)| is_rule(row, width))
        .map(|(index, _)| index);
    let (Some(bottom), Some(top)) = (rules.next(), rules.next()) else {
        return InputBox::Unrecognized;
    };
    let inner = &rows[top + 1..bottom];
    let Some(first) = inner.first() else {
        return InputBox::Unrecognized;
    };
    let Some(first_rest) = first.trim_start().strip_prefix(PROMPT) else {
        return InputBox::Unrecognized;
    };
    if inner.len() == 1 && first_rest.trim().is_empty() {
        InputBox::EmptyBusy
    } else {
        InputBox::NotEmpty
    }
}

fn read_comm(pid: u32) -> Option<String> {
    let comm = fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
    Some(comm.trim_end_matches('\n').to_owned())
}

/// Fields of `/proc/<pid>/stat` after the last `)` (`comm` may contain spaces
/// and parentheses); the first one is field 3 (`state`).
fn stat_fields_after_comm(stat: &str) -> Option<std::str::SplitWhitespace<'_>> {
    let close = stat.rfind(')')?;
    Some(stat[close + 1..].split_whitespace())
}

fn parse_ppid(stat: &str) -> Option<u32> {
    stat_fields_after_comm(stat)?.nth(1)?.parse().ok()
}

fn parse_starttime(stat: &str) -> Option<u64> {
    stat_fields_after_comm(stat)?.nth(19)?.parse().ok()
}

fn read_stat(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/stat")).ok()
}

/// Generation of `pid` when it is a `claude` process, `None` otherwise or when
/// `/proc` cannot be read.
pub(crate) fn claude_generation(pid: u32) -> Option<ProcGen> {
    if read_comm(pid)? != "claude" {
        return None;
    }
    let starttime = parse_starttime(&read_stat(pid)?)?;
    Some(ProcGen { pid, starttime })
}

/// `comm` of a Codex process, or of a wrapper named after it (`codex-x`).
fn is_codex_comm(comm: &str) -> bool {
    comm.starts_with("codex")
}

fn read_proc_node(pid: u32) -> Option<(String, u32)> {
    Some((read_comm(pid)?, parse_ppid(&read_stat(pid)?)?))
}

/// Walk `ppid` from `sender` up to `leader`, reading `(comm, ppid)` of a pid
/// through `read`. Fails closed: any unreadable node, a Codex process on the
/// path, a loop, or running out of steps yields `false`.
fn origin_allowed_with(
    sender: u32,
    leader: u32,
    read: impl Fn(u32) -> Option<(String, u32)>,
) -> bool {
    let mut current = sender;
    for _ in 0..MAX_WALK_STEPS {
        if current == 0 {
            return false;
        }
        let Some((comm, ppid)) = read(current) else {
            return false;
        };
        if is_codex_comm(&comm) {
            return false;
        }
        if current == leader {
            return true;
        }
        if ppid == current {
            return false;
        }
        current = ppid;
    }
    false
}

/// Whether `sender_pid` is the leader or its descendant with no Codex process
/// on the way.
pub(crate) fn origin_allowed(sender_pid: u32, leader_pid: u32) -> bool {
    origin_allowed_with(sender_pid, leader_pid, read_proc_node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    fn rule() -> String {
        RULE.to_string().repeat(120)
    }

    fn rows(tail: &[&str]) -> Vec<String> {
        tail.iter().map(|row| (*row).to_owned()).collect()
    }

    fn screen(inner: &[&str]) -> Vec<String> {
        let mut out = rows(&["", "  Ctrl+Y to paste deleted text"]);
        out.push(rule());
        out.extend(rows(inner));
        out.push(rule());
        out.extend(rows(&[
            "  ◆ ws  │    │ Sonnet 5.5/H",
            "  +0/-0 │ 9s",
            "  ⠀",
            "  ⏵⏵ auto mode on (shift+tab to cycle)",
        ]));
        out
    }

    #[test]
    fn empty_prompt_during_hold_is_empty_busy() {
        assert_eq!(classify_input_box(&screen(&["❯"])), InputBox::EmptyBusy);
        assert_eq!(classify_input_box(&screen(&["❯\u{a0}"])), InputBox::EmptyBusy);
    }

    #[test]
    fn prompt_with_text_is_not_empty() {
        assert_eq!(classify_input_box(&screen(&["❯\u{a0}abc"])), InputBox::NotEmpty);
    }

    #[test]
    fn multiline_input_is_not_empty() {
        let inner = ["❯\u{a0}line1", "  line2", "  line3"];
        assert_eq!(classify_input_box(&screen(&inner)), InputBox::NotEmpty);
        assert_eq!(classify_input_box(&screen(&["❯", "  line2"])), InputBox::NotEmpty);
    }

    #[test]
    fn idle_placeholder_is_not_empty() {
        let inner = ["❯\u{a0}Try \"write a test for <filepath>\""];
        assert_eq!(classify_input_box(&screen(&inner)), InputBox::NotEmpty);
    }

    #[test]
    fn no_rules_is_unrecognized() {
        let screen = rows(&["$ ls", "file", "❯", "  status"]);
        assert_eq!(classify_input_box(&screen), InputBox::Unrecognized);
        assert_eq!(classify_input_box(&[]), InputBox::Unrecognized);
    }

    #[test]
    fn narrow_rule_is_unrecognized() {
        let mut screen = screen(&["❯"]);
        let narrow = RULE.to_string().repeat(40);
        for row in screen.iter_mut().filter(|row| row.starts_with(RULE)) {
            *row = narrow.clone();
        }
        // Rows are right-trimmed, so a full-width row stands for the window width.
        screen.push("x".repeat(120));
        assert_eq!(classify_input_box(&screen), InputBox::Unrecognized);
    }

    #[test]
    fn partial_or_odd_boxes_are_unrecognized() {
        // Only the bottom rule is visible (window cut the box).
        let cut = rows(&["❯", &rule(), "  status"]);
        assert_eq!(classify_input_box(&cut), InputBox::Unrecognized);
        // Adjacent rules: nothing between them.
        let adjacent = rows(&[&rule(), &rule(), "  status"]);
        assert_eq!(classify_input_box(&adjacent), InputBox::Unrecognized);
        // Between the rules is not a prompt (e.g. a menu).
        assert_eq!(classify_input_box(&screen(&["  1. Yes"])), InputBox::Unrecognized);
        // Alt-screen style content without rules.
        assert_eq!(classify_input_box(&rows(&["alt1", "alt2", "", ""])), InputBox::Unrecognized);
    }

    #[test]
    fn rule_row_needs_only_rule_chars() {
        let mut broken = screen(&["❯"]);
        let bottom = broken.iter().rposition(|row| row.starts_with(RULE)).unwrap();
        broken[bottom].push('x');
        assert_eq!(classify_input_box(&broken), InputBox::Unrecognized);
    }

    #[test]
    fn stat_parsing_survives_odd_comm() {
        let stat = "123 (a) b (c d) S 77 1 1 0 -1 4194560 1 2 3 4 5 6 7 8 20 0 1 0 987654 1 2 3";
        assert_eq!(parse_ppid(stat), Some(77));
        assert_eq!(parse_starttime(stat), Some(987654));
        assert_eq!(parse_ppid("garbage"), None);
        assert_eq!(parse_starttime("1 (x) S 1"), None);
    }

    fn table(nodes: &[(u32, &str, u32)]) -> impl Fn(u32) -> Option<(String, u32)> {
        let map: HashMap<u32, (String, u32)> = nodes
            .iter()
            .map(|(pid, comm, ppid)| (*pid, ((*comm).to_owned(), *ppid)))
            .collect();
        move |pid| map.get(&pid).cloned()
    }

    #[test]
    fn walk_reaches_leader_and_rejects_codex_loops_and_gaps() {
        let ok = [(30, "sleep", 20), (20, "sh", 10), (10, "claude", 1), (1, "init", 0)];
        assert!(origin_allowed_with(30, 10, table(&ok)));
        assert!(origin_allowed_with(10, 10, table(&ok)));
        // Not a descendant of the leader.
        assert!(!origin_allowed_with(30, 99, table(&ok)));

        let codex = [(30, "sleep", 20), (20, "codex", 10), (10, "claude", 1), (1, "init", 0)];
        assert!(!origin_allowed_with(30, 10, table(&codex)));
        let codex_sender = [(30, "codex", 10), (10, "claude", 1)];
        assert!(!origin_allowed_with(30, 10, table(&codex_sender)));

        // Unreadable node on the way.
        let gap = [(30, "sleep", 20), (10, "claude", 1)];
        assert!(!origin_allowed_with(30, 10, table(&gap)));

        // Loop that never reaches the leader.
        let looped = [(30, "a", 20), (20, "b", 30)];
        assert!(!origin_allowed_with(30, 10, table(&looped)));
        let selfloop = [(30, "a", 30)];
        assert!(!origin_allowed_with(30, 10, table(&selfloop)));

        // Chain longer than 64 steps is refused even if it would reach the leader.
        let long = |pid: u32| Some(("sh".to_owned(), pid + 1));
        assert!(!origin_allowed_with(1, 100, long));
        assert!(origin_allowed_with(40, 100, long));
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = std::env::temp_dir().join(format!(
                "ronsole-tab-input-{tag}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        /// Copy of `/bin/sh` under the given name (its `comm` becomes `name`).
        fn shell_named(&self, name: &str) -> PathBuf {
            let path = self.0.join(name);
            fs::copy("/bin/sh", &path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
            path
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Reaper(Child);

    impl Drop for Reaper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Spawn `program -c script`, retrying while the freshly copied binary is
    /// still held open for writing by a concurrent fork (`ETXTBSY`).
    fn spawn_shell(program: &Path, script: &str) -> Reaper {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match Command::new(program)
                .args(["-c", script])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => return Reaper(child),
                Err(err) if err.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(err) => panic!("spawn {} failed: {err}", program.display()),
            }
        }
    }

    /// Find a live process with the given `comm` whose ancestry includes `root`.
    fn wait_for_descendant(root: u32, comm: &str) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(entries) = fs::read_dir("/proc") {
                for entry in entries.flatten() {
                    let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok())
                    else {
                        continue;
                    };
                    if pid != root
                        && read_comm(pid).as_deref() == Some(comm)
                        && is_descendant(pid, root)
                    {
                        return pid;
                    }
                }
            }
            assert!(Instant::now() < deadline, "no `{comm}` descendant of {root}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn is_descendant(mut pid: u32, root: u32) -> bool {
        for _ in 0..MAX_WALK_STEPS {
            let Some((_, ppid)) = read_proc_node(pid) else {
                return false;
            };
            if ppid == root {
                return true;
            }
            if ppid <= 1 {
                return false;
            }
            pid = ppid;
        }
        false
    }

    #[test]
    fn descendant_of_claude_process_is_allowed() {
        let dir = TestDir::new("allowed");
        let claude = dir.shell_named("claude");
        let leader = spawn_shell(&claude, "sleep 30; true");
        let sleeper = wait_for_descendant(leader.0.id(), "sleep");
        assert!(origin_allowed(sleeper, leader.0.id()));
        assert!(origin_allowed(leader.0.id(), leader.0.id()));
    }

    #[test]
    fn chain_through_codex_named_process_is_refused() {
        let dir = TestDir::new("codex");
        let claude = dir.shell_named("claude");
        let codex = dir.shell_named("codex-x");
        let script = format!("{} -c 'sleep 30; true'; true", codex.display());
        let leader = spawn_shell(&claude, &script);
        let sleeper = wait_for_descendant(leader.0.id(), "sleep");
        assert!(!origin_allowed(sleeper, leader.0.id()));
    }

    #[test]
    fn unrelated_process_is_refused() {
        let dir = TestDir::new("unrelated");
        let claude = dir.shell_named("claude");
        let leader = spawn_shell(&claude, "sleep 30; true");
        let outsider = spawn_shell(Path::new("/bin/sh"), "sleep 30; true");
        let outsider_sleep = wait_for_descendant(outsider.0.id(), "sleep");
        assert!(!origin_allowed(outsider_sleep, leader.0.id()));
        assert!(!origin_allowed(outsider.0.id(), leader.0.id()));
        // Dead / non-existent pid and pid 0 fail closed.
        assert!(!origin_allowed(u32::MAX, leader.0.id()));
        assert!(!origin_allowed(0, leader.0.id()));
    }

    #[test]
    fn claude_generation_requires_claude_comm_and_tracks_restarts() {
        assert_eq!(claude_generation(std::process::id()), None);
        assert_eq!(claude_generation(u32::MAX), None);

        let dir = TestDir::new("generation");
        let claude = dir.shell_named("claude");
        let first = spawn_shell(&claude, "sleep 30; true");
        let first_id = first.0.id();
        let first_gen = loop {
            if let Some(generation) = claude_generation(first_id) {
                break generation;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(first_gen.pid, first_id);
        assert!(first_gen.starttime > 0);
        assert_eq!(claude_generation(first_id), Some(first_gen));
        drop(first);
        assert_eq!(claude_generation(first_id), None);

        // Start times tick at 10 ms; make sure the restart lands in a later tick.
        std::thread::sleep(Duration::from_millis(50));
        let second = spawn_shell(&claude, "sleep 30; true");
        let second_id = second.0.id();
        let second_gen = loop {
            if let Some(generation) = claude_generation(second_id) {
                break generation;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_ne!(second_gen.starttime, first_gen.starttime);
    }
}
