//! Headless rendering: the runtime drives frames into an offscreen EGL pbuffer.

pub(crate) mod backend;
mod keys;
mod protocol;
mod session;

use crate::app::{
    App, AutomationOptions, HeadlessRunError, automation_options_from_args,
    write_automation_startup_failure,
};
use crate::launch::TerminalLaunchSpec;
use protocol::{parse_scale, parse_size};
use session::{FrameBudget, HzSource, ProfileChoice, SessionOptions};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::process::ExitCode;

/// Physical size of the `--pgo-train` headless surface.
const PGO_TRAIN_WIDTH: u32 = 1280;
const PGO_TRAIN_HEIGHT: u32 = 800;
/// Default physical size of a protocol session (spec §4.1).
const DEFAULT_SESSION_SIZE: (u32, u32) = (PGO_TRAIN_WIDTH, PGO_TRAIN_HEIGHT);
/// Frame rate of a protocol session when neither `--hz` nor `--budget-ms` is given.
const DEFAULT_HZ: f64 = 240.0;
/// Number of `-<n>` suffixes tried before a profile directory is reported as busy.
const PROFILE_SUFFIX_LIMIT: u32 = 1000;

/// Entry point for `ronsole --headless ...`. Never opens the single-instance socket
/// or connects to Wayland.
pub(crate) fn main(args: &[OsString]) -> ExitCode {
    match parse_args(args) {
        Ok(Mode::Pgo(options)) => run_pgo(options),
        Ok(Mode::Protocol(protocol)) => run_protocol_mode(protocol),
        Err(error) => {
            eprintln!("headless: {error}");
            ExitCode::from(2)
        }
    }
}

/// Runs the command protocol over `--script FILE`, or over stdin when no script is given.
/// Responses go to stdout; an unreadable script is an input error (exit 1).
fn run_protocol_mode(protocol: ProtocolArgs) -> ExitCode {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let code = match protocol.script {
        Some(path) => match fs::File::open(&path) {
            Ok(file) => session::run_protocol(
                protocol.options,
                io::BufReader::new(file),
                &mut output,
            ),
            Err(error) => {
                eprintln!("headless: script {}: {error}", path.display());
                1
            }
        },
        None => session::run_protocol(protocol.options, io::stdin().lock(), &mut output),
    };
    ExitCode::from(code)
}

/// Runs the `--pgo-train` automation loop until its own exit.
fn run_pgo(options: AutomationOptions) -> ExitCode {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let profile = match Profile::create(runtime_dir, std::process::id()) {
        Ok(profile) => profile,
        Err(error) => {
            let message = error.to_string();
            eprintln!("headless: profile: {message}");
            let _ = write_automation_startup_failure(&options, "headless-profile", &message);
            return ExitCode::FAILURE;
        }
    };
    crate::prefer_egl_vendor();
    crate::tune_glibc_allocator();
    // The receiver stays alive for the whole run and is never read: recorded links are discarded.
    let (mut app, _receiver) = match App::load_headless(Some(options.clone()), &profile.dir, None) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("headless: {error}");
            return ExitCode::FAILURE;
        }
    };
    match app.run_headless(
        (PGO_TRAIN_WIDTH, PGO_TRAIN_HEIGHT),
        1.0,
        TerminalLaunchSpec::default(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(HeadlessRunError::Egl(error)) => {
            eprintln!("headless: {error}");
            let _ = write_automation_startup_failure(&options, "headless-egl", &error);
            ExitCode::from(3)
        }
        Err(HeadlessRunError::Run(error)) => {
            eprintln!("headless: {error}");
            ExitCode::FAILURE
        }
    }
}

/// What `ronsole --headless` runs: the `--pgo-train` loop or the command protocol.
enum Mode {
    Pgo(AutomationOptions),
    Protocol(ProtocolArgs),
}

/// Protocol-mode arguments: where commands are read from and how the session starts.
struct ProtocolArgs {
    /// `None` reads commands from stdin.
    script: Option<PathBuf>,
    options: SessionOptions,
}

/// Parses `ronsole --headless ...` argv. `--pgo-train` selects the PGO options parser;
/// any other argument list runs the command protocol.
fn parse_args(args: &[OsString]) -> Result<Mode, String> {
    if args.get(1).map(OsString::as_os_str) != Some(OsStr::new("--headless")) {
        return Err("expected --headless as the first argument".to_string());
    }
    let automation_args: Vec<OsString> = args
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, arg)| arg.clone())
        .collect();
    if automation_args
        .iter()
        .skip(1)
        .any(|arg| arg == OsStr::new("--pgo-train"))
    {
        return automation_options_from_args(&automation_args).map(Mode::Pgo);
    }
    parse_protocol_args(&automation_args[1..]).map(Mode::Protocol)
}

/// Parses the protocol flags of spec §4.1; every argument must be one of them.
fn parse_protocol_args(values: &[OsString]) -> Result<ProtocolArgs, String> {
    let texts = values
        .iter()
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| "protocol argument is not UTF-8".to_string())
        })
        .collect::<Result<Vec<&str>, String>>()?;
    let mut script = None;
    let mut size = DEFAULT_SESSION_SIZE;
    let mut scale = 1.0_f32;
    let mut profile_dir = None;
    let mut keep_profile = false;
    let mut shell = None;
    let mut hz = None;
    let mut budget_ms = None;
    let mut index = 0;
    while index < texts.len() {
        let flag = texts[index];
        index += 1;
        match flag {
            "--keep-profile" => keep_profile = true,
            "--script" | "--size" | "--scale" | "--profile" | "--shell" | "--hz"
            | "--budget-ms" => {
                let value = texts
                    .get(index)
                    .copied()
                    .ok_or_else(|| format!("{flag} needs a value"))?;
                index += 1;
                match flag {
                    "--script" => script = Some(PathBuf::from(value)),
                    "--size" => size = parse_size(value)?,
                    "--scale" => scale = parse_scale(value)?,
                    "--profile" => profile_dir = Some(PathBuf::from(value)),
                    "--shell" => shell = Some(PathBuf::from(value)),
                    "--hz" => hz = Some(parse_positive(flag, value)?),
                    _ => budget_ms = Some(parse_positive(flag, value)?),
                }
            }
            _ => return Err(format!("unknown protocol argument: {flag}")),
        }
    }
    if keep_profile && profile_dir.is_some() {
        return Err("--keep-profile cannot be combined with --profile".to_string());
    }
    if hz.is_some() && budget_ms.is_some() {
        return Err("--hz and --budget-ms are mutually exclusive".to_string());
    }
    let budget = match (hz, budget_ms) {
        (Some(hz), None) => FrameBudget {
            budget_ms: 1000.0 / hz,
            hz,
            source: HzSource::Arg,
        },
        (None, Some(budget_ms)) => FrameBudget {
            budget_ms,
            hz: 1000.0 / budget_ms,
            source: HzSource::Arg,
        },
        _ => FrameBudget {
            budget_ms: 1000.0 / DEFAULT_HZ,
            hz: DEFAULT_HZ,
            source: HzSource::Default,
        },
    };
    let profile = match profile_dir {
        Some(dir) => ProfileChoice::Dir(dir),
        None => ProfileChoice::Temp {
            keep: keep_profile,
        },
    };
    Ok(ProtocolArgs {
        script,
        options: SessionOptions {
            size,
            scale,
            profile,
            shell,
            budget,
        },
    })
}

/// Parses a finite number above zero for `flag`.
fn parse_positive(flag: &str, value: &str) -> Result<f64, String> {
    let number: f64 = value
        .parse()
        .map_err(|_| format!("{flag} expects a number"))?;
    if number.is_finite() && number > 0.0 {
        Ok(number)
    } else {
        Err(format!("{flag} must be a positive number"))
    }
}

/// Per-run directory holding the headless config; removed when dropped.
pub(crate) struct Profile {
    pub(crate) dir: PathBuf,
    /// Whether dropping the profile removes `dir`.
    remove_on_drop: bool,
}

impl Profile {
    /// Uses a caller-owned directory, creating it when missing. Its files stay after drop.
    pub(crate) fn existing(dir: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            remove_on_drop: false,
        })
    }

    /// Keeps the directory after drop (`--keep-profile`).
    pub(crate) fn keep(mut self) -> Self {
        self.remove_on_drop = false;
        self
    }

    /// Creates `<runtime_dir or /tmp>/ronsole-headless-<pid>`, adding `-<n>` while
    /// the name is taken. The directory is created with mode 0700.
    pub(crate) fn create(runtime_dir: Option<PathBuf>, pid: u32) -> io::Result<Self> {
        let base = runtime_dir.unwrap_or_else(|| PathBuf::from("/tmp"));
        for suffix in 0..=PROFILE_SUFFIX_LIMIT {
            let name = if suffix == 0 {
                format!("ronsole-headless-{pid}")
            } else {
                format!("ronsole-headless-{pid}-{suffix}")
            };
            let dir = base.join(name);
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    return Ok(Self {
                        dir,
                        remove_on_drop: true,
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "no free headless profile directory name",
        ))
    }
}

impl Drop for Profile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_protocol;
