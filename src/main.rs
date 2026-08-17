#[cfg(not(target_os = "linux"))]
compile_error!("Ronsole supports Linux/Wayland only");

mod app;
mod config;
mod input;
mod input_types;
mod launch;
mod platform;
mod renderer;
mod runtime;
mod scroll;
mod search;
mod single_line_input;
mod tabs;
pub mod terminal;
mod terminal_compat;
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
    let (automation_options, initial_launch): (Option<app::AutomationOptions>, _) =
        match launch::parse_startup_args_with_current_dir(&args, env::current_dir) {
            Ok(launch::StartupMode::Normal(launch)) => (None, launch),
            Ok(launch::StartupMode::PgoAutomation) => {
                let options = match app::automation_options_from_args(&args) {
                    Ok(options) => options,
                    Err(error) => {
                        eprintln!("Ronsole: invalid PGO automation arguments: {error}");
                        return ExitCode::FAILURE;
                    }
                };
                (Some(options), launch::TerminalLaunchSpec::default())
            }
            Err(error) => {
                eprintln!("Ronsole: invalid arguments: {error}");
                return ExitCode::FAILURE;
            }
        };
    let pgo_training = automation_options.is_some();

    let single_instance = platform::single_instance::acquire_single_instance(&initial_launch);
    let mut primary_instance = match single_instance {
        Ok(platform::single_instance::SingleInstanceStatus::Primary(primary)) => primary,
        Ok(platform::single_instance::SingleInstanceStatus::Secondary) => {
            if let Some(options) = automation_options.as_ref() {
                let error = "PGO training connected to an existing Ronsole single-instance socket";
                if let Err(report_error) =
                    app::write_automation_startup_failure(options, "single-instance", error)
                {
                    eprintln!("Ronsole: failed to write PGO startup report: {report_error}");
                }
                eprintln!("Ronsole: {error}");
                return ExitCode::FAILURE;
            }
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("Ronsole: single-instance startup failed: {error}");
            if let Some(options) = automation_options.as_ref() {
                let message = format!("single-instance startup failed: {error}");
                if let Err(report_error) =
                    app::write_automation_startup_failure(options, "single-instance", &message)
                {
                    eprintln!("Ronsole: failed to write PGO startup report: {report_error}");
                }
            }
            return ExitCode::FAILURE;
        }
    };

    prefer_egl_vendor();
    tune_glibc_allocator();

    let mut app = if let Some(options) = automation_options {
        match app::App::load_with_automation(options) {
            Ok(app) => app,
            Err(error) => {
                eprintln!("Ronsole: PGO automation startup failed: {error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        app::App::load()
    };
    if let Err(error) = app.run_direct_wayland(&mut primary_instance, initial_launch) {
        eprintln!("Ronsole: Wayland runtime failed: {error}");
        if pgo_training {
            return ExitCode::FAILURE;
        }
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
    fn startup_cli_classification_precedes_single_instance_and_wayland_startup() {
        let source = include_str!("main.rs");
        let capture = source
            .find("let args = env::args_os().collect::<Vec<_>>()")
            .expect("startup must capture raw argv once");
        let classify = source
            .find("launch::parse_startup_args_with_current_dir(&args, env::current_dir)")
            .expect("startup mode classifier must remain present");
        let pgo = source
            .find("app::automation_options_from_args(&args)")
            .expect("PGO parser must consume the captured argv");
        let gate = source
            .find("acquire_single_instance(&initial_launch)")
            .expect("single-instance gate must remain present");
        let direct_loop = source
            .find("app.run_direct_wayland(&mut primary_instance, initial_launch)")
            .expect("direct Wayland app loop must remain present");
        assert!(capture < classify);
        assert!(classify < pgo);
        assert!(pgo < gate);
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
