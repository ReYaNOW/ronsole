use std::ffi::{OsStr, OsString};
use std::io;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TerminalLaunchSpec {
    pub(crate) working_directory: Option<PathBuf>,
    pub(crate) command: Vec<OsString>,
    pub(crate) hold: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StartupMode {
    Normal(TerminalLaunchSpec),
    PgoAutomation,
}

enum NormalCliToken<'a> {
    WorkingDirectory {
        option: &'a OsString,
        value: Option<&'a OsString>,
    },
    Hold,
    Execute(&'a [OsString]),
    Other(&'a OsString),
}

fn normal_cli_token(args: &[OsString], index: usize) -> (NormalCliToken<'_>, usize) {
    let arg = &args[index];
    if arg == OsStr::new("--workdir") || arg == OsStr::new("--working-directory") {
        return (
            NormalCliToken::WorkingDirectory {
                option: arg,
                value: args.get(index + 1),
            },
            index.saturating_add(2).min(args.len()),
        );
    }
    if arg == OsStr::new("--hold") || arg == OsStr::new("--noclose") {
        return (NormalCliToken::Hold, index + 1);
    }
    if arg == OsStr::new("-e") {
        return (
            NormalCliToken::Execute(args.get(index + 1..).unwrap_or_default()),
            args.len(),
        );
    }
    (NormalCliToken::Other(arg), index + 1)
}

fn pgo_automation_requested(args: &[OsString]) -> bool {
    let mut index = 1usize;
    while index < args.len() {
        let (token, next_index) = normal_cli_token(args, index);
        match token {
            NormalCliToken::Execute(_) => return false,
            NormalCliToken::Other(arg) if arg == OsStr::new("--pgo-train") => return true,
            _ => index = next_index,
        }
    }
    false
}

pub(crate) fn parse_startup_args_with_current_dir<F>(
    args: &[OsString],
    current_dir: F,
) -> Result<StartupMode, String>
where
    F: Fn() -> io::Result<PathBuf>,
{
    if pgo_automation_requested(args) {
        return Ok(StartupMode::PgoAutomation);
    }
    parse_terminal_launch_args_with_current_dir(args, current_dir).map(StartupMode::Normal)
}

fn parse_terminal_launch_args_with_current_dir<F>(
    args: &[OsString],
    current_dir: F,
) -> Result<TerminalLaunchSpec, String>
where
    F: Fn() -> io::Result<PathBuf>,
{
    let mut launch = TerminalLaunchSpec::default();
    let mut index = 1usize;
    while index < args.len() {
        let (token, next_index) = normal_cli_token(args, index);
        match token {
            NormalCliToken::WorkingDirectory { option, value } => {
                if launch.working_directory.is_some() {
                    return Err("duplicate --workdir/--working-directory".to_string());
                }
                let value = value
                    .ok_or_else(|| format!("{} requires a directory", option.to_string_lossy()))?;
                let mut path = PathBuf::from(value.as_os_str());
                if path.is_relative() {
                    let launcher_cwd = current_dir().map_err(|error| {
                        format!("failed to resolve relative working directory: {error}")
                    })?;
                    path = launcher_cwd.join(path);
                }
                launch.working_directory = Some(path);
            }
            NormalCliToken::Hold => launch.hold = true,
            NormalCliToken::Execute(command) => {
                if command.is_empty() {
                    return Err("-e requires a command".to_string());
                }
                launch.command = command.to_vec();
                break;
            }
            NormalCliToken::Other(arg) => {
                return Err(format!(
                    "unknown Ronsole argument: {}",
                    arg.to_string_lossy()
                ));
            }
        }
        index = next_index;
    }
    Ok(launch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;
    use std::path::Path;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn parse(values: &[&str]) -> Result<TerminalLaunchSpec, String> {
        parse_terminal_launch_args_with_current_dir(&args(values), || {
            Ok(PathBuf::from("/launcher/cwd"))
        })
    }

    fn parse_startup(values: &[&str]) -> Result<StartupMode, String> {
        parse_startup_args_with_current_dir(&args(values), || Ok(PathBuf::from("/launcher/cwd")))
    }

    #[test]
    fn default_launch_spec_preserves_default_terminal_session() {
        assert_eq!(
            TerminalLaunchSpec::default(),
            TerminalLaunchSpec {
                working_directory: None,
                command: Vec::new(),
                hold: false,
            }
        );
        let parsed = parse_terminal_launch_args_with_current_dir(&args(&["ronsole"]), || {
            panic!("default launch must not inspect process cwd")
        })
        .unwrap();
        assert_eq!(parsed, TerminalLaunchSpec::default());
    }

    #[test]
    fn workdir_options_preserve_absolute_paths_and_alias() {
        let explicit = parse_terminal_launch_args_with_current_dir(
            &args(&["ronsole", "--workdir", "/tmp"]),
            || panic!("absolute workdir must not inspect process cwd"),
        )
        .unwrap();
        assert_eq!(
            explicit.working_directory.as_deref(),
            Some(Path::new("/tmp"))
        );

        let alias = parse(&["ronsole", "--working-directory", "/var/tmp"]).unwrap();
        assert_eq!(
            alias.working_directory.as_deref(),
            Some(Path::new("/var/tmp"))
        );
    }

    #[test]
    fn relative_workdir_uses_injected_launcher_cwd_without_canonicalizing() {
        let parsed = parse(&["ronsole", "--workdir", "target/../session"]).unwrap();
        assert_eq!(
            parsed.working_directory.as_deref(),
            Some(Path::new("/launcher/cwd/target/../session"))
        );
    }

    #[test]
    fn hold_and_noclose_are_idempotent_aliases() {
        assert!(parse(&["ronsole", "--hold"]).unwrap().hold);
        assert!(parse(&["ronsole", "--noclose"]).unwrap().hold);
        assert!(
            parse(&["ronsole", "--hold", "--noclose", "--hold"])
                .unwrap()
                .hold
        );
    }

    #[test]
    fn execute_option_takes_all_remaining_raw_arguments() {
        let parsed = parse(&["ronsole", "-e", "htop", "-d", "10"]).unwrap();
        assert_eq!(parsed.command, args(&["htop", "-d", "10"]));

        let child_option = parse(&["ronsole", "-e", "foo", "--workdir", "/x"]).unwrap();
        assert_eq!(child_option.command, args(&["foo", "--workdir", "/x"]));
        assert_eq!(child_option.working_directory, None);
    }

    #[test]
    fn startup_mode_does_not_reinterpret_execute_remainder_as_pgo() {
        let parsed = parse_startup(&["ronsole", "-e", "foo", "--pgo-train"]).unwrap();
        assert_eq!(
            parsed,
            StartupMode::Normal(TerminalLaunchSpec {
                working_directory: None,
                command: args(&["foo", "--pgo-train"]),
                hold: false,
            })
        );

        let pgo_option =
            parse_startup(&["ronsole", "-e", "foo", "--pgo-workspace", "/tmp/x"]).unwrap();
        assert_eq!(
            pgo_option,
            StartupMode::Normal(TerminalLaunchSpec {
                working_directory: None,
                command: args(&["foo", "--pgo-workspace", "/tmp/x"]),
                hold: false,
            })
        );
    }

    #[test]
    fn startup_mode_preserves_normal_options_before_execute_boundary() {
        let held = parse_startup(&["ronsole", "--hold", "-e", "foo", "--pgo-train"]).unwrap();
        assert_eq!(
            held,
            StartupMode::Normal(TerminalLaunchSpec {
                working_directory: None,
                command: args(&["foo", "--pgo-train"]),
                hold: true,
            })
        );

        let workdir =
            parse_startup(&["ronsole", "--workdir", "/tmp", "-e", "foo", "--pgo-train"]).unwrap();
        assert_eq!(
            workdir,
            StartupMode::Normal(TerminalLaunchSpec {
                working_directory: Some(PathBuf::from("/tmp")),
                command: args(&["foo", "--pgo-train"]),
                hold: false,
            })
        );
    }

    #[test]
    fn startup_mode_skips_normal_option_values_when_detecting_pgo() {
        let parsed = parse_startup(&["ronsole", "--workdir", "--pgo-train"]).unwrap();
        assert_eq!(
            parsed,
            StartupMode::Normal(TerminalLaunchSpec {
                working_directory: Some(PathBuf::from("/launcher/cwd/--pgo-train")),
                command: Vec::new(),
                hold: false,
            })
        );
    }

    #[test]
    fn startup_mode_detects_frozen_pgo_invocation_before_normal_parsing() {
        let parsed = parse_startup(&[
            "ronsole",
            "--pgo-train",
            "--pgo-workspace",
            "/tmp/workspace",
            "--pgo-report",
            "/tmp/report.json",
            "--pgo-timeout-seconds",
            "120",
        ])
        .unwrap();
        assert_eq!(parsed, StartupMode::PgoAutomation);
    }

    #[test]
    fn parser_preserves_non_utf8_workdir_and_command_bytes() {
        let raw_workdir = OsString::from_vec(vec![b'd', b'i', b'r', 0xff]);
        let raw_command = OsString::from_vec(vec![b'f', b'o', b'o', 0xfe]);
        let raw_arg = OsString::from_vec(vec![b'a', b'r', b'g', 0xfd]);
        let argv = vec![
            OsString::from("ronsole"),
            OsString::from("--workdir"),
            raw_workdir.clone(),
            OsString::from("-e"),
            raw_command.clone(),
            raw_arg.clone(),
        ];
        let parsed =
            parse_terminal_launch_args_with_current_dir(&argv, || Ok(PathBuf::from("/launcher")))
                .unwrap();
        assert_eq!(
            parsed.working_directory,
            Some(PathBuf::from("/launcher").join(raw_workdir))
        );
        assert_eq!(parsed.command, vec![raw_command, raw_arg]);
    }

    #[test]
    fn parser_rejects_missing_values_duplicates_and_unknown_arguments() {
        for invalid in [
            args(&["ronsole", "-e"]),
            args(&["ronsole", "--workdir"]),
            args(&["ronsole", "--working-directory"]),
            args(&[
                "ronsole",
                "--workdir",
                "/tmp",
                "--working-directory",
                "/var/tmp",
            ]),
            args(&["ronsole", "--unknown"]),
            args(&["ronsole", "unexpected"]),
        ] {
            assert!(
                parse_terminal_launch_args_with_current_dir(&invalid, || {
                    Ok(PathBuf::from("/launcher"))
                })
                .is_err()
            );
        }
    }
}
