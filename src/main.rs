#[cfg(not(target_os = "linux"))]
compile_error!("Ronsole supports Linux/Wayland only");

mod app;
mod config;
mod platform;
mod renderer;
mod runtime;
mod scroll;
mod search;
mod single_line_input;
mod tabs;
mod input;
pub mod terminal;
mod terminal_compat;
mod terminal_process;

use std::env;
use std::path::Path;
use winit::event_loop::{ControlFlow, EventLoop};

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

fn main() {
    let mut primary_instance = match platform::single_instance::acquire_single_instance() {
        Ok(platform::single_instance::SingleInstanceStatus::Primary(primary)) => primary,
        Ok(platform::single_instance::SingleInstanceStatus::Secondary) => return,
        Err(error) => {
            eprintln!("Ronsole: single-instance startup failed: {error}");
            return;
        }
    };

    prefer_egl_vendor();
    tune_glibc_allocator();

    let event_loop = match EventLoop::<app::AppEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(error) => {
            eprintln!("Ronsole: failed to create Wayland event loop: {error}");
            return;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    if let Err(error) = primary_instance
        .start_listener(move || proxy.send_event(app::AppEvent::ExternalLaunch).is_ok())
    {
        eprintln!("Ronsole: failed to start single-instance listener: {error}");
        return;
    }

    let mut app = app::App::load();
    if let Err(error) = event_loop.run_app(&mut app) {
        eprintln!("Ronsole: event loop failed: {error}");
    }
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
    fn single_instance_gate_precedes_event_loop_and_app_creation() {
        let source = include_str!("main.rs");
        let gate = source
            .find("acquire_single_instance()")
            .expect("single-instance gate must remain present");
        let event_loop = source
            .find("EventLoop::<app::AppEvent>::with_user_event()")
            .expect("typed event loop must remain present");
        let app = source
            .find("app::App::load()")
            .expect("app construction must remain present");
        assert!(gate < event_loop);
        assert!(event_loop < app);
    }
}
