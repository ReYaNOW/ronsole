#[cfg(not(target_os = "linux"))]
compile_error!("Ronsole supports Linux/Wayland only");

mod app;
mod config;
mod input;
mod input_types;
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

    let mut app = app::App::load();
    if let Err(error) = app.run_direct_wayland(&mut primary_instance) {
        eprintln!("Ronsole: Wayland runtime failed: {error}");
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
    fn single_instance_gate_precedes_direct_wayland_app_loop() {
        let source = include_str!("main.rs");
        let gate = source
            .find("acquire_single_instance()")
            .expect("single-instance gate must remain present");
        let app = source
            .find("app::App::load()")
            .expect("app construction must remain present");
        let direct_loop = source
            .find("app.run_direct_wayland(&mut primary_instance)")
            .expect("direct Wayland app loop must remain present");
        assert!(gate < app);
        assert!(app < direct_loop);
    }
}
