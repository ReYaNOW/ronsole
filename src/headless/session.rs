//! Headless command session: owns the offscreen `App`, its profile and the input
//! state that protocol commands drive.

use std::path::PathBuf;

/// Where the session's profile directory lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProfileChoice {
    /// `${XDG_RUNTIME_DIR:-/tmp}/ronsole-headless-<pid>[-<n>]`, removed on exit unless `keep`.
    Temp { keep: bool },
    /// Caller-owned directory (`--profile DIR`); never removed.
    Dir(PathBuf),
}

/// Where the frame budget value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HzSource {
    /// Given explicitly with `--hz` or `--budget-ms`.
    Arg,
    /// Fallback default frame rate.
    Default,
}

/// Frame pacing budget of the session.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameBudget {
    pub(crate) budget_ms: f64,
    pub(crate) hz: f64,
    pub(crate) source: HzSource,
}

/// Options of one protocol session, parsed from the `--headless` flags.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionOptions {
    /// Physical size of the offscreen surface.
    pub(crate) size: (u32, u32),
    pub(crate) scale: f32,
    pub(crate) profile: ProfileChoice,
    pub(crate) shell: Option<PathBuf>,
    pub(crate) budget: FrameBudget,
}
