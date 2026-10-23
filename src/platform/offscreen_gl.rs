//! Surfaceless EGL context backed by a pbuffer, for headless GL rendering.
//!
//! libEGL is loaded with `dlopen` and kept mapped (`RTLD_NODELETE`), and the
//! EGL display is never terminated, so contexts created in the same process
//! stay valid regardless of the drop order.

use glow::HasContext;
use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr;

const ZERO_SIZE_ERROR: &str = "CreatePbuffer: zero size";

type Object = *mut c_void;
type GetProc = unsafe extern "C" fn(*const c_char) -> *const c_void;
type MakeCurrent = unsafe extern "C" fn(Object, Object, Object, Object) -> u32;
type Destroy = unsafe extern "C" fn(Object, Object) -> u32;

struct Library(Object);

impl Drop for Library {
    fn drop(&mut self) {
        unsafe {
            libc::dlclose(self.0);
        }
    }
}

unsafe fn symbol<T: Copy>(library: &Library, name: &CStr) -> Result<T, String> {
    let ptr = unsafe { libc::dlsym(library.0, name.as_ptr()) };
    if ptr.is_null() {
        return Err(format!("dlopen: missing {}", name.to_string_lossy()));
    }
    Ok(unsafe { std::mem::transmute_copy(&ptr) })
}

#[derive(Debug, Default)]
pub(crate) struct GlStrings {
    pub renderer: String,
    pub version: String,
    pub vendor: String,
}

pub(crate) struct OffscreenContext {
    _library: Library,
    display: Object,
    config: Object,
    surface: Object,
    context: Object,
    width: u32,
    height: u32,
    strings: GlStrings,
    get_proc: GetProc,
    get_error: unsafe extern "C" fn() -> u32,
    create_surface: unsafe extern "C" fn(Object, Object, *const i32) -> Object,
    make_current: MakeCurrent,
    destroy_surface: Destroy,
    destroy_context: Destroy,
}

impl OffscreenContext {
    /// Creates an OpenGL 3.3 Core context with a `width` x `height` pbuffer and
    /// makes it current on the calling thread.
    pub(crate) fn new(width: u32, height: u32) -> Result<Self, String> {
        if width == 0 || height == 0 {
            return Err(ZERO_SIZE_ERROR.into());
        }
        Self::setup(width, height).map_err(|error| format!("EGL setup failed at {error}"))
    }

    fn setup(width: u32, height: u32) -> Result<Self, String> {
        unsafe {
            let raw = libc::dlopen(
                c"libEGL.so.1".as_ptr(),
                libc::RTLD_NOW | libc::RTLD_LOCAL | libc::RTLD_NODELETE,
            );
            if raw.is_null() {
                let detail = libc::dlerror();
                let message = if detail.is_null() {
                    "libEGL.so.1 unavailable".into()
                } else {
                    CStr::from_ptr(detail).to_string_lossy().into_owned()
                };
                return Err(format!("dlopen: {message}"));
            }
            let library = Library(raw);
            macro_rules! load {
                ($name:expr, $ty:ty) => {
                    symbol::<$ty>(&library, $name)?
                };
            }
            let get_error = load!(c"eglGetError", unsafe extern "C" fn() -> u32);
            let query_string =
                load!(c"eglQueryString", unsafe extern "C" fn(Object, i32) -> *const c_char);
            let get_display = load!(
                c"eglGetPlatformDisplay",
                unsafe extern "C" fn(u32, Object, *const isize) -> Object
            );
            let initialize = load!(
                c"eglInitialize",
                unsafe extern "C" fn(Object, *mut i32, *mut i32) -> u32
            );
            let bind_api = load!(c"eglBindAPI", unsafe extern "C" fn(u32) -> u32);
            let choose_config = load!(
                c"eglChooseConfig",
                unsafe extern "C" fn(Object, *const i32, *mut Object, i32, *mut i32) -> u32
            );
            let create_surface = load!(
                c"eglCreatePbufferSurface",
                unsafe extern "C" fn(Object, Object, *const i32) -> Object
            );
            let create_context = load!(
                c"eglCreateContext",
                unsafe extern "C" fn(Object, Object, Object, *const i32) -> Object
            );
            let make_current = load!(c"eglMakeCurrent", MakeCurrent);
            let get_proc = load!(c"eglGetProcAddress", GetProc);
            let destroy_surface = load!(c"eglDestroySurface", Destroy);
            let destroy_context = load!(c"eglDestroyContext", Destroy);

            // EGL_EXTENSIONS queried on EGL_NO_DISPLAY returns the client extensions.
            let extensions_ptr = query_string(ptr::null_mut(), 0x3055);
            let extensions = if extensions_ptr.is_null() {
                String::new()
            } else {
                CStr::from_ptr(extensions_ptr).to_string_lossy().into_owned()
            };
            if !extensions
                .split_whitespace()
                .any(|ext| ext == "EGL_MESA_platform_surfaceless")
            {
                return Err(format!(
                    "GetPlatformDisplay: EGL_MESA_platform_surfaceless missing; client extensions: {extensions}"
                ));
            }
            // EGL_PLATFORM_SURFACELESS_MESA
            let display = get_display(0x31DD, ptr::null_mut(), ptr::null());
            if display.is_null() {
                return Err(format!(
                    "GetPlatformDisplay: eglGetError=0x{:04X}",
                    get_error()
                ));
            }
            let mut result = Self {
                _library: library,
                display,
                config: ptr::null_mut(),
                surface: ptr::null_mut(),
                context: ptr::null_mut(),
                width,
                height,
                strings: GlStrings::default(),
                get_proc,
                get_error,
                create_surface,
                make_current,
                destroy_surface,
                destroy_context,
            };
            let (mut major, mut minor) = (0, 0);
            if initialize(display, &mut major, &mut minor) != 1 {
                return Err(result.error("Initialize"));
            }
            // EGL_OPENGL_API
            if bind_api(0x30A2) != 1 {
                return Err(result.error("BindAPI"));
            }
            // EGL_SURFACE_TYPE=PBUFFER_BIT, EGL_RENDERABLE_TYPE=EGL_OPENGL_BIT, RGB 8 bits
            let attrs = [0x3033, 1, 0x3040, 8, 0x3024, 8, 0x3023, 8, 0x3022, 8, 0x3038];
            let mut count = 0;
            if choose_config(display, attrs.as_ptr(), &mut result.config, 1, &mut count) != 1
                || count != 1
            {
                return Err(result.error("ChooseConfig"));
            }
            result.surface = result.create_pbuffer(width, height);
            if result.surface.is_null() {
                return Err(result.error("CreatePbuffer"));
            }
            // EGL_CONTEXT_MAJOR_VERSION=3, EGL_CONTEXT_MINOR_VERSION=3,
            // EGL_CONTEXT_OPENGL_PROFILE_MASK=EGL_CONTEXT_OPENGL_CORE_PROFILE_BIT
            let context_attrs = [0x3098, 3, 0x30FB, 3, 0x30FD, 1, 0x3038];
            result.context = create_context(
                display,
                result.config,
                ptr::null_mut(),
                context_attrs.as_ptr(),
            );
            if result.context.is_null() {
                return Err(result.error("CreateContext"));
            }
            result.bind(result.surface)?;
            let gl = result.glow();
            result.strings = GlStrings {
                renderer: gl.get_parameter_string(glow::RENDERER),
                version: gl.get_parameter_string(glow::VERSION),
                vendor: gl.get_parameter_string(glow::VENDOR),
            };
            Ok(result)
        }
    }

    fn error(&self, stage: &str) -> String {
        format!("{stage}: eglGetError=0x{:04X}", unsafe {
            (self.get_error)()
        })
    }

    fn create_pbuffer(&self, width: u32, height: u32) -> Object {
        // EGL_WIDTH, EGL_HEIGHT, EGL_NONE
        let attrs = [0x3057, width as i32, 0x3056, height as i32, 0x3038];
        unsafe { (self.create_surface)(self.display, self.config, attrs.as_ptr()) }
    }

    fn bind(&self, surface: Object) -> Result<(), String> {
        if unsafe { (self.make_current)(self.display, surface, surface, self.context) } != 1 {
            return Err(self.error("MakeCurrent"));
        }
        Ok(())
    }

    pub(crate) fn glow(&self) -> glow::Context {
        unsafe {
            glow::Context::from_loader_function(|name| {
                let Ok(name) = CString::new(name) else {
                    return ptr::null();
                };
                (self.get_proc)(name.as_ptr())
            })
        }
    }

    /// Makes the context current on the calling thread with its pbuffer.
    pub(crate) fn make_current(&self) -> Result<(), String> {
        self.bind(self.surface)
    }

    /// Replaces the pbuffer. The old surface is destroyed only after the new one is bound.
    pub(crate) fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Err(ZERO_SIZE_ERROR.into());
        }
        let next = self.create_pbuffer(width, height);
        if next.is_null() {
            return Err(self.error("CreatePbuffer"));
        }
        if let Err(error) = self.bind(next) {
            unsafe { (self.destroy_surface)(self.display, next) };
            return Err(error);
        }
        unsafe { (self.destroy_surface)(self.display, self.surface) };
        self.surface = next;
        self.width = width;
        self.height = height;
        Ok(())
    }

    pub(crate) fn requested_context(&self) -> String {
        "EGL surfaceless OpenGL 3.3 Core".into()
    }

    pub(crate) fn gl_strings(&self) -> &GlStrings {
        &self.strings
    }

    pub(crate) fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
}

impl Drop for OffscreenContext {
    fn drop(&mut self) {
        // Release the thread binding, then destroy the context, then the surface.
        // The display is intentionally not terminated: other contexts may share it.
        if !self.context.is_null() {
            unsafe {
                (self.make_current)(
                    self.display,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                );
                (self.destroy_context)(self.display, self.context);
            }
        }
        if !self.surface.is_null() {
            unsafe { (self.destroy_surface)(self.display, self.surface) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glow::HasContext;

    fn new_test_context(width: u32, height: u32) -> OffscreenContext {
        crate::prefer_egl_vendor();
        OffscreenContext::new(width, height).expect("offscreen EGL context")
    }

    #[test]
    fn zero_size_is_rejected_before_egl() {
        assert_eq!(
            OffscreenContext::new(0, 8).err().as_deref(),
            Some("CreatePbuffer: zero size")
        );
    }

    #[test]
    fn offscreen_context_reports_size_and_resizes() {
        let mut ctx = new_test_context(64, 48);
        assert_eq!(ctx.size(), (64, 48));
        assert!(!ctx.gl_strings().renderer.is_empty());
        assert!(!ctx.gl_strings().version.is_empty());
        assert!(!ctx.gl_strings().vendor.is_empty());
        assert_eq!(ctx.requested_context(), "EGL surfaceless OpenGL 3.3 Core");
        ctx.resize(128, 96).expect("resize pbuffer");
        assert_eq!(ctx.size(), (128, 96));
    }

    #[test]
    fn second_context_survives_first_drop() {
        let a = new_test_context(32, 32);
        let b = new_test_context(32, 32);
        drop(a);
        b.make_current()
            .expect("second context current after first drop");
        let gl = b.glow();
        let mut pixel = [0u8; 4];
        unsafe {
            gl.clear_color(1.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
            gl.read_pixels(
                0,
                0,
                1,
                1,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut pixel)),
            );
        }
        assert_eq!(pixel, [255, 0, 0, 255]);
    }

    #[test]
    fn resize_failure_keeps_old_surface() {
        let mut ctx = new_test_context(16, 16);
        assert!(ctx.resize(0, 0).is_err());
        assert_eq!(ctx.size(), (16, 16));
        ctx.make_current().expect("old surface still current");
        // u32::MAX as i32 == -1: Mesa rejects the pbuffer (CreatePbuffer failure keeps the old one).
        assert!(ctx.resize(u32::MAX, 16).is_err());
        assert_eq!(ctx.size(), (16, 16));
        ctx.make_current().expect("old surface survives EGL failure");
    }
}
