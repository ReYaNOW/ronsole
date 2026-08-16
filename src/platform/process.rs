use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
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
}
