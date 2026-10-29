#[cfg(not(target_os = "linux"))]
compile_error!("Ronsole supports Linux/Wayland only");

mod app;
mod claude_close;
mod config;
mod headless;
mod input;
mod input_types;
mod launch;
mod panels;
mod platform;
mod renderer;
mod runtime;
mod scroll;
mod search;
mod single_line_input;
mod tab_input;
mod tabs;
pub mod terminal;
mod terminal_process;
mod wake;
mod wayland_input;

use std::env;
use std::path::Path;
use std::process::ExitCode;

const EGL_VENDOR_ENV: &str = "__EGL_VENDOR_LIBRARY_FILENAMES";
const RONSOLE_EGL_VENDOR_ENV: &str = "RONSOLE_EGL_VENDOR";
const NVIDIA_EGL_VENDOR: &str = "/usr/share/glvnd/egl_vendor.d/10_nvidia.json";
const MESA_EGL_VENDOR: &str = "/usr/share/glvnd/egl_vendor.d/50_mesa.json";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EglVendorPreference {
    Auto,
    System,
    Nvidia,
    Mesa,
}

fn parse_egl_vendor_preference(raw: Option<&str>) -> EglVendorPreference {
    let Some(value) = raw.map(str::trim) else {
        return EglVendorPreference::Auto;
    };
    if value.eq_ignore_ascii_case("auto") {
        EglVendorPreference::Auto
    } else if value.eq_ignore_ascii_case("system") {
        EglVendorPreference::System
    } else if value.eq_ignore_ascii_case("nvidia") {
        EglVendorPreference::Nvidia
    } else if value.eq_ignore_ascii_case("mesa") {
        EglVendorPreference::Mesa
    } else {
        EglVendorPreference::System
    }
}

fn preferred_egl_vendor_path(
    preference: EglVendorPreference,
    nvidia_present: bool,
) -> Option<&'static str> {
    match preference {
        EglVendorPreference::System => None,
        EglVendorPreference::Nvidia => Some(NVIDIA_EGL_VENDOR),
        EglVendorPreference::Mesa => Some(MESA_EGL_VENDOR),
        EglVendorPreference::Auto if nvidia_present => Some(NVIDIA_EGL_VENDOR),
        EglVendorPreference::Auto => None,
    }
}

fn prefer_egl_vendor() {
    if env::var_os(EGL_VENDOR_ENV).is_some() {
        return;
    }

    let preference = env::var_os(RONSOLE_EGL_VENDOR_ENV)
        .and_then(|value| value.into_string().ok())
        .map(|value| parse_egl_vendor_preference(Some(&value)))
        .unwrap_or(EglVendorPreference::Auto);
    let Some(vendor_path) = preferred_egl_vendor_path(preference, nvidia_gpu_present()) else {
        return;
    };
    if !Path::new(vendor_path).exists() {
        return;
    }

    // Must happen before glutin causes EGL/GLVND to load.
    unsafe {
        env::set_var(EGL_VENDOR_ENV, vendor_path);
    }
}

fn nvidia_gpu_present() -> bool {
    if Path::new("/dev/nvidiactl").exists() || Path::new("/proc/driver/nvidia/version").exists() {
        return true;
    }

    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return false;
    };
    for entry in entries.flatten() {
        let vendor_path = entry.path().join("device/vendor");
        let Ok(vendor) = std::fs::read_to_string(vendor_path) else {
            continue;
        };
        if vendor.trim().eq_ignore_ascii_case("0x10de") {
            return true;
        }
    }
    false
}

fn tune_glibc_allocator() {
    unsafe extern "C" {
        fn mallopt(param: i32, val: i32) -> i32;
    }

    // glibc M_ARENA_MAX = -8. Two arenas avoid excessive startup arena growth.
    unsafe {
        let _ = mallopt(-8, 2);
    }
}

fn main() -> ExitCode {
    let args = env::args_os().collect::<Vec<_>>();
    // Answered by querying the running instance; must not start the GUI or
    // take part in single-instance external launch.
    if args.get(1).is_some_and(|arg| arg == "tab-status") {
        return platform::single_instance::run_tab_status_command();
    }
    // Headless runs never touch the single-instance socket or Wayland.
    if args.get(1).is_some_and(|arg| arg == "--headless") {
        return headless::main(&args);
    }
    if launch::pgo_train_without_headless(&args) {
        eprintln!("ronsole: --pgo-train requires --headless");
        return ExitCode::from(2);
    }
    let initial_launch =
        match launch::parse_startup_args_with_current_dir(&args, env::current_dir) {
            Ok(launch) => launch,
            Err(error) => {
                eprintln!("Ronsole: invalid arguments: {error}");
                return ExitCode::FAILURE;
            }
        };

    let single_instance = platform::single_instance::acquire_single_instance(&initial_launch);
    let mut primary_instance = match single_instance {
        Ok(platform::single_instance::SingleInstanceStatus::Primary(primary)) => primary,
        Ok(platform::single_instance::SingleInstanceStatus::Secondary) => return ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Ronsole: single-instance startup failed: {error}");
            return ExitCode::FAILURE;
        }
    };

    prefer_egl_vendor();
    tune_glibc_allocator();

    let mut app = app::App::load();
    if let Err(error) = app.run_direct_wayland(&mut primary_instance, initial_launch) {
        eprintln!("Ronsole: Wayland runtime failed: {error}");
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn egl_vendor_preference_parser_is_conservative() {
        assert_eq!(parse_egl_vendor_preference(None), EglVendorPreference::Auto);
        assert_eq!(
            parse_egl_vendor_preference(Some(" AUTO ")),
            EglVendorPreference::Auto
        );
        assert_eq!(
            parse_egl_vendor_preference(Some("nViDiA")),
            EglVendorPreference::Nvidia
        );
        assert_eq!(
            parse_egl_vendor_preference(Some("mesa")),
            EglVendorPreference::Mesa
        );
        assert_eq!(
            parse_egl_vendor_preference(Some("system")),
            EglVendorPreference::System
        );
        assert_eq!(
            parse_egl_vendor_preference(Some("unknown")),
            EglVendorPreference::System
        );
    }

    #[test]
    fn egl_vendor_auto_only_forces_nvidia_when_detected() {
        assert_eq!(
            preferred_egl_vendor_path(EglVendorPreference::Auto, true),
            Some(NVIDIA_EGL_VENDOR)
        );
        assert_eq!(
            preferred_egl_vendor_path(EglVendorPreference::Auto, false),
            None
        );
        assert_eq!(
            preferred_egl_vendor_path(EglVendorPreference::Mesa, true),
            Some(MESA_EGL_VENDOR)
        );
        assert_eq!(
            preferred_egl_vendor_path(EglVendorPreference::System, true),
            None
        );
    }

    #[test]
    fn headless_branch_precedes_single_instance_and_wayland_startup() {
        let source = include_str!("main.rs");
        let headless = source
            .find("headless::main(&args)")
            .expect("headless branch must remain present");
        let classify = source
            .find("launch::parse_startup_args_with_current_dir(&args, env::current_dir)")
            .expect("startup mode classifier must remain present");
        let gate = source
            .find("acquire_single_instance(&initial_launch)")
            .expect("single-instance gate must remain present");
        assert!(headless < classify);
        assert!(headless < gate);
    }

    #[test]
    fn startup_cli_classification_precedes_single_instance_and_wayland_startup() {
        let source = include_str!("main.rs");
        let capture = source
            .find("let args = env::args_os().collect::<Vec<_>>()")
            .expect("startup must capture raw argv once");
        let classify = source
            .find("launch::parse_startup_args_with_current_dir(&args, env::current_dir)")
            .expect("startup mode classifier must remain present");
        let headless = source
            .find("headless::main(&args)")
            .expect("headless branch must remain present");
        let pgo_check = source
            .find("launch::pgo_train_without_headless(&args)")
            .expect("--pgo-train without --headless check must consume the captured argv");
        let gate = source
            .find("acquire_single_instance(&initial_launch)")
            .expect("single-instance gate must remain present");
        let direct_loop = source
            .find("app.run_direct_wayland(&mut primary_instance, initial_launch)")
            .expect("direct Wayland app loop must remain present");
        assert!(capture < headless);
        assert!(headless < pgo_check);
        assert!(pgo_check < classify);
        assert!(classify < gate);
        assert!(gate < direct_loop);
    }

    #[test]
    fn single_instance_gate_precedes_direct_wayland_app_loop() {
        let source = include_str!("main.rs");
        let gate = source
            .find("acquire_single_instance(&initial_launch)")
            .expect("single-instance gate must remain present");
        let app = source
            .find("app::App::load()")
            .expect("app construction must remain present");
        let direct_loop = source
            .find("app.run_direct_wayland(&mut primary_instance, initial_launch)")
            .expect("direct Wayland app loop must remain present");
        assert!(gate < app);
        assert!(app < direct_loop);
    }
}
