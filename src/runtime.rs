use crate::input_types::{CursorKind, KeyInput, KeyState, Modifiers};
use crate::renderer::Renderer;
use crate::wake::WakeHandle;
use crate::wayland_input::{
    CursorShape, ImeBatch, KeyRepeatState, PointerAxisFrame, PointerAxisSource, RepeatConfig,
    WaylandInputEvent, XkbKeyboard, cursor_shape_for_kind, physical_key_from_evdev,
    pointer_button_from_linux, pointer_position, push_bounded_input_event, should_emit_xkb_text,
};
use glutin::config::{Config, ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, GlContext, GlProfile, NotCurrentContext,
    NotCurrentGlContext, PossiblyCurrentContext, Priority, Version,
};
use glutin::display::{Display as GlutinDisplay, DisplayApiPreference, GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SwapInterval, WindowSurface};
use raw_window_handle::{
    RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle,
};
use std::cell::{Cell, RefCell};
use std::cmp::Reverse;
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};
use wayland_client::backend::WaylandError;
use wayland_client::protocol::{
    wl_callback, wl_compositor, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_cursor::{CursorImageBuffer, CursorTheme};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3, zwp_text_input_v3,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

const WAYLAND_COMPOSITOR_MAX_VERSION: u32 = 6;
const WAYLAND_SEAT_MAX_VERSION: u32 = 9;
const WAYLAND_SHM_VERSION: u32 = 1;
const WAYLAND_APP_ID: &str = "ronsole";
const WAYLAND_TITLE: &str = "Ronsole";
const FRACTIONAL_SCALE_DENOMINATOR: f32 = 120.0;
const DEFAULT_CURSOR_SIZE: u32 = 24;
const MAX_CURSOR_BASE_SIZE: u32 = 128;
const MAX_CURSOR_SCALE_BUCKET: u32 = 8;
const MAX_CURSOR_THEME_SIZE: u32 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GlContextPlan {
    Desktop { major: u8, minor: u8 },
    Gles { major: u8, minor: u8 },
}

impl GlContextPlan {
    fn label(self) -> &'static str {
        match self {
            Self::Desktop { major: 4, minor: 1 } => "OpenGL 4.1 Core",
            Self::Desktop { major: 3, minor: 3 } => "OpenGL 3.3 Core",
            Self::Gles { major: 3, minor: 0 } => "OpenGL ES 3.0",
            _ => "unsupported graphics plan",
        }
    }

    fn attributes(
        self,
        raw_window_handle: RawWindowHandle,
        priority: GlContextPriorityRequest,
    ) -> glutin::context::ContextAttributes {
        let version = match self {
            Self::Desktop { major, minor } | Self::Gles { major, minor } => {
                Version::new(major, minor)
            }
        };
        let builder = match self {
            Self::Desktop { .. } => ContextAttributesBuilder::new()
                .with_profile(GlProfile::Core)
                .with_context_api(ContextApi::OpenGl(Some(version))),
            Self::Gles { .. } => {
                ContextAttributesBuilder::new().with_context_api(ContextApi::Gles(Some(version)))
            }
        };
        let builder = match priority {
            GlContextPriorityRequest::High => builder.with_priority(Priority::High),
            GlContextPriorityRequest::Default => builder,
        };
        builder.build(Some(raw_window_handle))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GlContextPriorityRequest {
    High,
    Default,
}

impl GlContextPriorityRequest {
    fn label(self) -> &'static str {
        match self {
            Self::High => "High",
            Self::Default => "Default",
        }
    }
}

fn gl_context_plans() -> &'static [GlContextPlan] {
    const PLANS: &[GlContextPlan] = &[
        GlContextPlan::Desktop { major: 4, minor: 1 },
        GlContextPlan::Desktop { major: 3, minor: 3 },
        GlContextPlan::Gles { major: 3, minor: 0 },
    ];
    PLANS
}

fn gl_context_priority_requests() -> &'static [GlContextPriorityRequest] {
    const PRIORITIES: &[GlContextPriorityRequest] = &[
        GlContextPriorityRequest::High,
        GlContextPriorityRequest::Default,
    ];
    PRIORITIES
}

fn gl_context_attempts() -> impl Iterator<Item = (GlContextPlan, GlContextPriorityRequest)> {
    gl_context_plans().iter().copied().flat_map(|plan| {
        gl_context_priority_requests()
            .iter()
            .copied()
            .map(move |priority| (plan, priority))
    })
}

fn framebuffer_config_rank(hardware_accelerated: bool, num_samples: u8) -> (bool, Reverse<u8>) {
    (hardware_accelerated, Reverse(num_samples))
}

fn blocking_swap_interval() -> SwapInterval {
    SwapInterval::Wait(NonZeroU32::MIN)
}

fn create_not_current_context(
    gl_config: &Config,
    raw_window_handle: RawWindowHandle,
) -> Result<(NotCurrentContext, GlContextPlan, GlContextPriorityRequest), String> {
    let display = gl_config.display();
    let mut errors = String::with_capacity(384);
    for (plan, priority) in gl_context_attempts() {
        let attributes = plan.attributes(raw_window_handle, priority);
        match unsafe { display.create_context(gl_config, &attributes) } {
            Ok(context) => return Ok((context, plan, priority)),
            Err(error) => {
                use std::fmt::Write as _;
                let _ = writeln!(
                    errors,
                    "{} / {} priority: {error}",
                    plan.label(),
                    priority.label()
                );
            }
        }
    }
    Err(format!(
        "failed to create supported graphics context:\n{errors}"
    ))
}

fn create_surface_and_context(
    gl_config: &Config,
    raw_window_handle: RawWindowHandle,
    width: u32,
    height: u32,
    not_current_context: NotCurrentContext,
) -> Result<(Surface<WindowSurface>, PossiblyCurrentContext, bool), String> {
    let attributes = glutin::surface::SurfaceAttributesBuilder::<WindowSurface>::new().build(
        raw_window_handle,
        nonzero_dimension(width),
        nonzero_dimension(height),
    );
    let display = gl_config.display();
    let surface = unsafe { display.create_window_surface(gl_config, &attributes) }
        .map_err(|error| format!("window surface creation failed: {error}"))?;
    let context = not_current_context
        .make_current(&surface)
        .map_err(|error| format!("making OpenGL context current failed: {error}"))?;
    let swap_interval_applied = surface
        .set_swap_interval(&context, blocking_swap_interval())
        .is_ok();
    Ok((surface, context, swap_interval_applied))
}

fn create_glow_context(gl_config: &Config) -> glow::Context {
    let display = gl_config.display();
    unsafe {
        glow::Context::from_loader_function(|symbol| {
            std::ffi::CString::new(symbol)
                .map(|symbol| display.get_proc_address(symbol.as_c_str()) as *const _)
                .unwrap_or(std::ptr::null())
        })
    }
}

fn trim_allocator_after_gl_bootstrap() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> i32;
    }
    unsafe {
        let _ = malloc_trim(0);
    }
}

#[inline]
fn nonzero_dimension(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value.max(1)).unwrap_or(NonZeroU32::MIN)
}

#[inline]
fn logical_dimension(value: f64) -> u32 {
    if !value.is_finite() || value <= 0.0 {
        return 1;
    }
    value.round().clamp(1.0, f64::from(i32::MAX)) as u32
}

pub(crate) fn logical_to_physical(
    logical_width: u32,
    logical_height: u32,
    scale_factor: f32,
) -> (u32, u32) {
    let scale = if scale_factor.is_finite() && scale_factor > 0.0 {
        f64::from(scale_factor)
    } else {
        1.0
    };
    let scaled = |value: u32| {
        (f64::from(value) * scale)
            .round()
            .clamp(0.0, f64::from(u32::MAX)) as u32
    };
    (scaled(logical_width), scaled(logical_height))
}

#[inline]
fn configured_dimension(current: u32, suggested: i32) -> u32 {
    if suggested > 0 {
        suggested as u32
    } else {
        current
    }
}

#[inline]
fn surface_renderable(configured: bool, physical_width: u32, physical_height: u32) -> bool {
    configured && physical_width > 0 && physical_height > 0
}

pub(crate) fn poll_timeout_millis(deadline: Option<Instant>, now: Instant) -> i32 {
    let Some(deadline) = deadline else {
        return -1;
    };
    if deadline <= now {
        return 0;
    }
    let duration = deadline.duration_since(now);
    let whole = duration.as_millis();
    let rounded = if duration > Duration::from_millis(whole.min(u128::from(u64::MAX)) as u64) {
        whole.saturating_add(1)
    } else {
        whole
    };
    rounded.min(i32::MAX as u128) as i32
}

fn choose_direct_gl_config(display: &GlutinDisplay) -> Result<Config, String> {
    let template = ConfigTemplateBuilder::new()
        .with_transparency(false)
        .with_depth_size(0)
        .with_stencil_size(0)
        .build();
    let configs = unsafe { display.find_configs(template) }
        .map_err(|error| format!("failed to enumerate EGL framebuffer configurations: {error}"))?;
    configs
        .max_by_key(|config| {
            framebuffer_config_rank(config.hardware_accelerated(), config.num_samples())
        })
        .ok_or_else(|| "no EGL framebuffer configuration is available".to_string())
}

fn raw_wayland_window(surface: &wl_surface::WlSurface) -> Result<RawWindowHandle, String> {
    let pointer = NonNull::new(surface.id().as_ptr().cast())
        .ok_or_else(|| "Wayland surface pointer is null".to_string())?;
    Ok(RawWindowHandle::Wayland(WaylandWindowHandle::new(pointer)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivationRequestPlan {
    Unavailable,
    ActivateSuppliedToken,
    RequestToken,
    AwaitPendingToken,
    MissingFallbackContext,
}

fn activation_request_plan(
    extension_available: bool,
    supplied_token: bool,
    token_request_pending: bool,
    fallback_context_available: bool,
) -> ActivationRequestPlan {
    if !extension_available {
        ActivationRequestPlan::Unavailable
    } else if supplied_token {
        ActivationRequestPlan::ActivateSuppliedToken
    } else if token_request_pending {
        ActivationRequestPlan::AwaitPendingToken
    } else if fallback_context_available {
        ActivationRequestPlan::RequestToken
    } else {
        ActivationRequestPlan::MissingFallbackContext
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorApplyPath {
    Shape(CursorShape),
    Fallback { scale_bucket: u32 },
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorFallbackStatus {
    Uninitialized,
    Ready { scale_bucket: u32 },
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorFallbackPreparation {
    ShapePath,
    Prepare { scale_bucket: u32 },
    Ready,
    Disabled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorFallbackImageSlot {
    Default,
    Pointer,
    Text,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CursorApplyPlan {
    serial: u32,
    kind: CursorKind,
    path: CursorApplyPath,
}

#[inline]
fn cursor_scale_bucket(scale_factor: f32) -> u32 {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return 1;
    }
    scale_factor
        .ceil()
        .clamp(1.0, MAX_CURSOR_SCALE_BUCKET as f32) as u32
}

fn cursor_apply_plan(
    serial: Option<u32>,
    kind: CursorKind,
    scale_factor: f32,
    cursor_shape_available: bool,
    fallback_scale_bucket: Option<u32>,
) -> Option<CursorApplyPlan> {
    let serial = serial?;
    if cursor_shape_available {
        return Some(CursorApplyPlan {
            serial,
            kind,
            path: CursorApplyPath::Shape(cursor_shape_for_kind(kind)),
        });
    }
    let scale_bucket = cursor_scale_bucket(scale_factor);
    Some(CursorApplyPlan {
        serial,
        kind,
        path: if fallback_scale_bucket == Some(scale_bucket) {
            CursorApplyPath::Fallback { scale_bucket }
        } else {
            CursorApplyPath::Disabled
        },
    })
}

fn cursor_fallback_preparation(
    cursor_shape_available: bool,
    fallback_supported: bool,
    status: CursorFallbackStatus,
    scale_factor: f32,
) -> CursorFallbackPreparation {
    if cursor_shape_available {
        return CursorFallbackPreparation::ShapePath;
    }
    if matches!(status, CursorFallbackStatus::Disabled) || !fallback_supported {
        return CursorFallbackPreparation::Disabled;
    }

    let scale_bucket = cursor_scale_bucket(scale_factor);
    match status {
        CursorFallbackStatus::Ready {
            scale_bucket: ready_bucket,
        } if ready_bucket == scale_bucket => CursorFallbackPreparation::Ready,
        CursorFallbackStatus::Uninitialized | CursorFallbackStatus::Ready { .. } => {
            CursorFallbackPreparation::Prepare { scale_bucket }
        }
        CursorFallbackStatus::Disabled => CursorFallbackPreparation::Disabled,
    }
}

#[inline]
fn cursor_fallback_image_slot(kind: CursorKind) -> CursorFallbackImageSlot {
    match kind {
        CursorKind::Default => CursorFallbackImageSlot::Default,
        CursorKind::Pointer => CursorFallbackImageSlot::Pointer,
        CursorKind::Text => CursorFallbackImageSlot::Text,
    }
}

fn cursor_names_for_kind(kind: CursorKind) -> &'static [&'static str] {
    match kind {
        CursorKind::Default => &["default", "left_ptr"],
        CursorKind::Pointer => &["pointer", "hand2", "hand1", "default", "left_ptr"],
        CursorKind::Text => &["text", "xterm", "default", "left_ptr"],
    }
}

fn cursor_theme_spec() -> (String, u32) {
    let name = std::env::var("XCURSOR_THEME")
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "default".to_string());
    let size = std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|size| size.parse::<u32>().ok())
        .filter(|size| (1..=MAX_CURSOR_BASE_SIZE).contains(size))
        .unwrap_or(DEFAULT_CURSOR_SIZE);
    (name, size)
}

#[inline]
fn cursor_theme_size(base_size: u32, scale_bucket: u32) -> u32 {
    base_size
        .saturating_mul(scale_bucket.max(1))
        .clamp(1, MAX_CURSOR_THEME_SIZE)
}

fn load_cursor_image_from_theme(
    theme: &mut CursorTheme,
    kind: CursorKind,
) -> Result<CursorImageBuffer, CursorFallbackError> {
    for name in cursor_names_for_kind(kind) {
        match catch_unwind(AssertUnwindSafe(|| {
            theme
                .get_cursor(name)
                .and_then(|cursor| (cursor.image_count() > 0).then(|| cursor[0].clone()))
        })) {
            Ok(Some(image)) => return Ok(image),
            Ok(None) => {}
            Err(_) => return Err(CursorFallbackError::Panicked),
        }
    }
    Err(CursorFallbackError::Message(format!(
        "cursor theme does not provide an image for {kind:?}"
    )))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct WaylandWindowMetrics {
    pub(crate) logical_width: u32,
    pub(crate) logical_height: u32,
    pub(crate) physical_width: u32,
    pub(crate) physical_height: u32,
    pub(crate) scale_factor: f32,
}

impl WaylandWindowMetrics {
    fn new(logical_width: u32, logical_height: u32, scale_factor: f32) -> Self {
        let (physical_width, physical_height) =
            logical_to_physical(logical_width, logical_height, scale_factor);
        Self {
            logical_width,
            logical_height,
            physical_width,
            physical_height,
            scale_factor,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WaylandRuntimeEvents {
    pub(crate) configured: Option<WaylandWindowMetrics>,
    pub(crate) scale_changed: Option<WaylandWindowMetrics>,
    pub(crate) close_requested: bool,
    pub(crate) input: Vec<WaylandInputEvent>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WaylandPollOutcome {
    pub(crate) woke: bool,
    pub(crate) timed_out: bool,
}

#[derive(Default)]
struct FrameSchedule {
    requested: Cell<bool>,
    ready: Cell<bool>,
    callback_outstanding: Cell<bool>,
    presented: Cell<bool>,
}

impl FrameSchedule {
    fn request(&self) {
        self.requested.set(true);
        if !self.presented.get() {
            self.ready.set(true);
        }
    }

    fn take_ready(&self) -> bool {
        if !self.requested.get() || !self.ready.get() {
            return false;
        }
        self.requested.set(false);
        self.ready.set(false);
        true
    }

    fn mark_presented(&self) {
        self.presented.set(true);
    }

    fn should_arm_callback(&self) -> bool {
        self.requested.get()
            && self.presented.get()
            && !self.ready.get()
            && !self.callback_outstanding.get()
    }

    fn mark_callback_outstanding(&self) {
        self.callback_outstanding.set(true);
    }

    fn callback_done(&self) {
        self.callback_outstanding.set(false);
        self.ready.set(true);
    }

    #[cfg(test)]
    fn callback_outstanding(&self) -> bool {
        self.callback_outstanding.get()
    }

    fn ready_requested(&self) -> bool {
        self.requested.get() && self.ready.get()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CursorFallbackApplied {
    serial: u32,
    kind: CursorKind,
    scale_bucket: u32,
}

struct CursorFallbackImages {
    default: CursorImageBuffer,
    pointer: CursorImageBuffer,
    text: CursorImageBuffer,
}

impl CursorFallbackImages {
    fn load(theme: &mut CursorTheme) -> Result<Self, CursorFallbackError> {
        Ok(Self {
            default: load_cursor_image_from_theme(theme, CursorKind::Default)?,
            pointer: load_cursor_image_from_theme(theme, CursorKind::Pointer)?,
            text: load_cursor_image_from_theme(theme, CursorKind::Text)?,
        })
    }

    fn image(&self, kind: CursorKind) -> &CursorImageBuffer {
        match cursor_fallback_image_slot(kind) {
            CursorFallbackImageSlot::Default => &self.default,
            CursorFallbackImageSlot::Pointer => &self.pointer,
            CursorFallbackImageSlot::Text => &self.text,
        }
    }
}

struct CursorFallbackResources {
    surface: wl_surface::WlSurface,
    theme: CursorTheme,
    images: CursorFallbackImages,
    theme_name: String,
    base_size: u32,
    scale_bucket: u32,
    last_applied: Option<CursorFallbackApplied>,
}

enum CursorFallbackState {
    Uninitialized,
    Ready(Box<CursorFallbackResources>),
    Disabled,
}

enum CursorFallbackError {
    Message(String),
    Panicked,
}

struct WaylandState {
    registry: Option<wl_registry::WlRegistry>,
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    activation_global_name: Option<u32>,
    activation: Option<xdg_activation_v1::XdgActivationV1>,
    pending_activation_token: Option<xdg_activation_token_v1::XdgActivationTokenV1>,
    decoration_manager: Option<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
    fractional_scale_manager: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    viewporter: Option<wp_viewporter::WpViewporter>,
    seat_global_name: Option<u32>,
    seat: Option<wl_seat::WlSeat>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    text_input_manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    text_input: Option<zwp_text_input_v3::ZwpTextInputV3>,
    cursor_shape_manager: Option<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1>,
    cursor_shape_device: Option<wp_cursor_shape_device_v1::WpCursorShapeDeviceV1>,
    surface: Option<wl_surface::WlSurface>,
    xdg_surface: Option<xdg_surface::XdgSurface>,
    toplevel: Option<xdg_toplevel::XdgToplevel>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    fractional_scale: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    viewport: Option<wp_viewport::WpViewport>,
    logical_width: u32,
    logical_height: u32,
    scale_factor: f32,
    pending_toplevel_size: Option<(i32, i32)>,
    configured: bool,
    close_requested: bool,
    pending_configure: Option<WaylandWindowMetrics>,
    pending_scale_change: Option<WaylandWindowMetrics>,
    pending_input: VecDeque<WaylandInputEvent>,
    pointer_frame: VecDeque<WaylandInputEvent>,
    pointer_axes: PointerAxisFrame,
    pointer_enter_serial: Option<u32>,
    activation_serial: Option<u32>,
    desired_cursor: Cell<CursorKind>,
    cursor_fallback: RefCell<CursorFallbackState>,
    xkb: XkbKeyboard,
    modifiers: Modifiers,
    keyboard_focused: bool,
    text_input_enabled: bool,
    ime: ImeBatch,
    repeat: KeyRepeatState,
    frame: Rc<FrameSchedule>,
}

impl WaylandState {
    fn new(logical_width: u32, logical_height: u32) -> Result<Self, String> {
        Ok(Self {
            registry: None,
            compositor: None,
            shm: None,
            wm_base: None,
            activation_global_name: None,
            activation: None,
            pending_activation_token: None,
            decoration_manager: None,
            fractional_scale_manager: None,
            viewporter: None,
            seat_global_name: None,
            seat: None,
            keyboard: None,
            pointer: None,
            text_input_manager: None,
            text_input: None,
            cursor_shape_manager: None,
            cursor_shape_device: None,
            surface: None,
            xdg_surface: None,
            toplevel: None,
            decoration: None,
            fractional_scale: None,
            viewport: None,
            logical_width,
            logical_height,
            scale_factor: 1.0,
            pending_toplevel_size: None,
            configured: false,
            close_requested: false,
            pending_configure: None,
            pending_scale_change: None,
            pending_input: VecDeque::new(),
            pointer_frame: VecDeque::new(),
            pointer_axes: PointerAxisFrame::default(),
            pointer_enter_serial: None,
            activation_serial: None,
            desired_cursor: Cell::new(CursorKind::Default),
            cursor_fallback: RefCell::new(CursorFallbackState::Uninitialized),
            xkb: XkbKeyboard::new()?,
            modifiers: Modifiers::empty(),
            keyboard_focused: false,
            text_input_enabled: false,
            ime: ImeBatch::default(),
            repeat: KeyRepeatState::with_default_config(),
            frame: Rc::new(FrameSchedule::default()),
        })
    }

    fn metrics(&self) -> WaylandWindowMetrics {
        WaylandWindowMetrics::new(self.logical_width, self.logical_height, self.scale_factor)
    }

    fn fractional_scaling_active(&self) -> bool {
        self.fractional_scale.is_some() && self.viewport.is_some()
    }

    fn push_input(&mut self, event: WaylandInputEvent) {
        push_bounded_input_event(&mut self.pending_input, event);
    }

    fn push_pointer_frame(&mut self, event: WaylandInputEvent) {
        push_bounded_input_event(&mut self.pointer_frame, event);
    }

    fn flush_pointer_frame(&mut self) {
        while let Some(event) = self.pointer_frame.pop_front() {
            self.push_input(event);
        }
        if let Some(delta) = self.pointer_axes.take_scroll(self.scale_factor) {
            self.push_input(WaylandInputEvent::Scroll(delta));
        }
    }

    fn apply_surface_geometry(&self) {
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        if self.fractional_scaling_active() {
            if surface.version() >= 3 {
                surface.set_buffer_scale(1);
            }
            if let Some(viewport) = self.viewport.as_ref() {
                let width = self.logical_width.min(i32::MAX as u32) as i32;
                let height = self.logical_height.min(i32::MAX as u32) as i32;
                if width > 0 && height > 0 {
                    viewport.set_destination(width, height);
                }
            }
        } else if surface.version() >= 3 {
            let factor = self.scale_factor.round().clamp(1.0, i32::MAX as f32) as i32;
            surface.set_buffer_scale(factor);
        }
    }

    fn create_window(&mut self, qh: &QueueHandle<Self>) -> Result<(), String> {
        if self.surface.is_some() {
            return Ok(());
        }
        let compositor = self
            .compositor
            .as_ref()
            .cloned()
            .ok_or_else(|| "Wayland compositor global is unavailable".to_string())?;
        let wm_base = self
            .wm_base
            .as_ref()
            .cloned()
            .ok_or_else(|| "xdg_wm_base global is unavailable".to_string())?;

        let surface = compositor.create_surface(qh, ());
        let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());
        let toplevel = xdg_surface.get_toplevel(qh, ());
        toplevel.set_title(WAYLAND_TITLE.to_owned());
        toplevel.set_app_id(WAYLAND_APP_ID.to_owned());

        let decoration = self.decoration_manager.as_ref().map(|manager| {
            let decoration = manager.get_toplevel_decoration(&toplevel, qh, ());
            decoration.set_mode(zxdg_toplevel_decoration_v1::Mode::ServerSide);
            decoration
        });

        let (viewport, fractional_scale) = match (
            self.viewporter.as_ref(),
            self.fractional_scale_manager.as_ref(),
        ) {
            (Some(viewporter), Some(manager)) => (
                Some(viewporter.get_viewport(&surface, qh, ())),
                Some(manager.get_fractional_scale(&surface, qh, ())),
            ),
            _ => (None, None),
        };

        self.surface = Some(surface);
        self.xdg_surface = Some(xdg_surface);
        self.toplevel = Some(toplevel);
        self.decoration = decoration;
        self.viewport = viewport;
        self.fractional_scale = fractional_scale;
        self.apply_surface_geometry();
        if let Some(surface) = self.surface.as_ref() {
            surface.commit();
        }
        Ok(())
    }

    fn cancel_pending_activation_token(&mut self) {
        if let Some(token) = self.pending_activation_token.take() {
            token.destroy();
        }
    }

    fn release_activation(&mut self) {
        self.cancel_pending_activation_token();
        if let Some(activation) = self.activation.take() {
            activation.destroy();
        }
        self.activation_global_name = None;
    }

    fn activate_existing_window(&mut self, qh: &QueueHandle<Self>, supplied_token: Option<String>) {
        if self.keyboard_focused {
            return;
        }
        let supplied_token = supplied_token.filter(|token| !token.is_empty());
        let extension_available = self.activation.is_some() && self.surface.is_some();
        match activation_request_plan(
            extension_available,
            supplied_token.is_some(),
            self.pending_activation_token.is_some(),
            self.activation_serial.is_some() && self.seat.is_some(),
        ) {
            ActivationRequestPlan::Unavailable
            | ActivationRequestPlan::AwaitPendingToken
            | ActivationRequestPlan::MissingFallbackContext => {}
            ActivationRequestPlan::ActivateSuppliedToken => {
                self.cancel_pending_activation_token();
                if let (Some(activation), Some(surface), Some(token)) = (
                    self.activation.as_ref(),
                    self.surface.as_ref(),
                    supplied_token,
                ) {
                    activation.activate(token, surface);
                }
            }
            ActivationRequestPlan::RequestToken => {
                let (Some(activation), Some(surface)) =
                    (self.activation.as_ref(), self.surface.as_ref())
                else {
                    return;
                };
                let token = activation.get_activation_token(qh, ());
                token.set_app_id(WAYLAND_APP_ID.to_owned());
                token.set_surface(surface);
                if let (Some(serial), Some(seat)) = (self.activation_serial, self.seat.as_ref()) {
                    token.set_serial(serial, seat);
                }
                token.commit();
                self.pending_activation_token = Some(token);
            }
        }
    }

    fn set_keyboard_focus(&mut self, focused: bool) {
        if focused {
            self.keyboard_focused = true;
            self.cancel_pending_activation_token();
            self.repeat.stop();
            self.push_input(WaylandInputEvent::Focus(true));
            self.push_input(WaylandInputEvent::Modifiers(self.modifiers));
            return;
        }

        self.keyboard_focused = false;
        self.repeat.stop();
        self.disable_text_input();
        self.xkb.clear_modifiers();
        self.modifiers = Modifiers::empty();
        self.push_input(WaylandInputEvent::Modifiers(Modifiers::empty()));
        self.push_input(WaylandInputEvent::Focus(false));
    }

    fn ensure_keyboard(&mut self, qh: &QueueHandle<Self>) {
        if self.keyboard.is_some() {
            return;
        }
        let Some(seat) = self.seat.as_ref() else {
            return;
        };
        self.keyboard = Some(seat.get_keyboard(qh, ()));
        self.ensure_text_input(qh);
    }

    fn remove_keyboard(&mut self) {
        if self.keyboard_focused {
            self.set_keyboard_focus(false);
        } else {
            self.repeat.stop();
            self.disable_text_input();
        }
        if let Some(keyboard) = self.keyboard.take()
            && keyboard.version() >= 3
        {
            keyboard.release();
        }
    }

    fn ensure_pointer(&mut self, connection: &Connection, qh: &QueueHandle<Self>) {
        if self.pointer.is_some() {
            return;
        }
        let Some(seat) = self.seat.as_ref() else {
            return;
        };
        if self.surface.is_some() {
            self.prepare_cursor_fallback_if_needed(connection, qh);
        }
        self.pointer = Some(seat.get_pointer(qh, ()));
        self.ensure_cursor_shape(qh);
    }

    fn remove_pointer(&mut self) {
        let had_pointer_focus = self.pointer_enter_serial.take().is_some();
        if let Some(device) = self.cursor_shape_device.take() {
            device.destroy();
        }
        if let Some(pointer) = self.pointer.take()
            && pointer.version() >= 3
        {
            pointer.release();
        }
        self.pointer_frame.clear();
        self.pointer_axes = PointerAxisFrame::default();
        self.clear_cursor_fallback_applied();
        if had_pointer_focus {
            self.push_input(WaylandInputEvent::PointerLeave);
        }
    }

    fn ensure_text_input(&mut self, qh: &QueueHandle<Self>) {
        if self.text_input.is_some() || self.keyboard.is_none() {
            return;
        }
        let (Some(manager), Some(seat)) = (self.text_input_manager.as_ref(), self.seat.as_ref())
        else {
            return;
        };
        self.text_input = Some(manager.get_text_input(seat, qh, ()));
    }

    fn disable_text_input(&mut self) {
        if self.text_input_enabled
            && let Some(text_input) = self.text_input.as_ref()
        {
            text_input.disable();
            text_input.commit();
        }
        self.text_input_enabled = false;
        self.ime.clear();
    }

    fn ensure_cursor_shape(&mut self, qh: &QueueHandle<Self>) {
        if self.cursor_shape_device.is_some() {
            return;
        }
        let (Some(manager), Some(pointer)) =
            (self.cursor_shape_manager.as_ref(), self.pointer.as_ref())
        else {
            return;
        };
        self.cursor_shape_device = Some(manager.get_pointer(pointer, qh, ()));
    }

    fn set_cursor_kind(&self, kind: CursorKind) {
        self.desired_cursor.set(kind);
        self.apply_cursor();
    }

    fn cursor_fallback_scale_bucket(&self) -> Option<u32> {
        let fallback = self.cursor_fallback.borrow();
        match &*fallback {
            CursorFallbackState::Ready(resources) => Some(resources.scale_bucket),
            CursorFallbackState::Uninitialized | CursorFallbackState::Disabled => None,
        }
    }

    fn cursor_fallback_status(&self) -> CursorFallbackStatus {
        let fallback = self.cursor_fallback.borrow();
        match &*fallback {
            CursorFallbackState::Uninitialized => CursorFallbackStatus::Uninitialized,
            CursorFallbackState::Ready(resources) => CursorFallbackStatus::Ready {
                scale_bucket: resources.scale_bucket,
            },
            CursorFallbackState::Disabled => CursorFallbackStatus::Disabled,
        }
    }

    fn prepare_cursor_fallback_if_needed(&self, connection: &Connection, qh: &QueueHandle<Self>) {
        let status = self.cursor_fallback_status();
        match cursor_fallback_preparation(
            self.cursor_shape_manager.is_some(),
            self.shm.is_some() && self.compositor.is_some(),
            status,
            self.scale_factor,
        ) {
            CursorFallbackPreparation::Prepare { scale_bucket } => {
                let _ = self.prepare_cursor_fallback_resources(connection, qh, scale_bucket);
            }
            CursorFallbackPreparation::Disabled
                if !matches!(status, CursorFallbackStatus::Disabled) =>
            {
                self.disable_cursor_fallback(CursorFallbackError::Message(
                    "wl_shm or wl_compositor is unavailable for the legacy cursor path".to_string(),
                ));
            }
            CursorFallbackPreparation::ShapePath
            | CursorFallbackPreparation::Ready
            | CursorFallbackPreparation::Disabled => {}
        }
    }

    fn apply_cursor(&self) {
        let Some(plan) = cursor_apply_plan(
            self.pointer_enter_serial,
            self.desired_cursor.get(),
            self.scale_factor,
            self.cursor_shape_device.is_some(),
            self.cursor_fallback_scale_bucket(),
        ) else {
            return;
        };

        match plan.path {
            CursorApplyPath::Shape(shape) => {
                let Some(device) = self.cursor_shape_device.as_ref() else {
                    return;
                };
                let shape = match shape {
                    CursorShape::Default => wp_cursor_shape_device_v1::Shape::Default,
                    CursorShape::Pointer => wp_cursor_shape_device_v1::Shape::Pointer,
                    CursorShape::Text => wp_cursor_shape_device_v1::Shape::Text,
                };
                device.set_shape(plan.serial, shape);
            }
            CursorApplyPath::Fallback { scale_bucket } => {
                self.apply_cursor_fallback(plan.serial, plan.kind, scale_bucket)
            }
            CursorApplyPath::Disabled => {}
        }
    }

    fn apply_cursor_fallback(&self, serial: u32, kind: CursorKind, scale_bucket: u32) {
        let Some(pointer) = self.pointer.as_ref() else {
            return;
        };
        let mut fallback = self.cursor_fallback.borrow_mut();
        let CursorFallbackState::Ready(resources) = &mut *fallback else {
            return;
        };
        let applied = CursorFallbackApplied {
            serial,
            kind,
            scale_bucket,
        };
        if resources.last_applied == Some(applied) {
            return;
        }
        if resources.scale_bucket != scale_bucket {
            return;
        }

        let image = resources.images.image(kind);
        let (width, height) = image.dimensions();
        let (hotspot_x, hotspot_y) = image.hotspot();
        let surface = &resources.surface;
        let scale = i32::try_from(scale_bucket).unwrap_or(1).max(1);
        if surface.version() >= 3 {
            surface.set_buffer_scale(scale);
        }
        surface.attach(Some(image), 0, 0);
        let damage_width = width.min(i32::MAX as u32) as i32;
        let damage_height = height.min(i32::MAX as u32) as i32;
        if surface.version() >= 4 {
            surface.damage_buffer(0, 0, damage_width, damage_height);
        } else {
            surface.damage(
                0,
                0,
                damage_width.saturating_add(scale - 1) / scale,
                damage_height.saturating_add(scale - 1) / scale,
            );
        }
        surface.commit();
        pointer.set_cursor(
            serial,
            Some(surface),
            (hotspot_x.min(i32::MAX as u32) as i32) / scale,
            (hotspot_y.min(i32::MAX as u32) as i32) / scale,
        );
        resources.last_applied = Some(applied);
    }

    fn prepare_cursor_fallback_resources(
        &self,
        connection: &Connection,
        qh: &QueueHandle<Self>,
        scale_bucket: u32,
    ) -> bool {
        let existing_spec = {
            let fallback = self.cursor_fallback.borrow();
            match &*fallback {
                CursorFallbackState::Disabled => return false,
                CursorFallbackState::Ready(resources) if resources.scale_bucket == scale_bucket => {
                    return true;
                }
                CursorFallbackState::Ready(resources) => {
                    Some((resources.theme_name.clone(), resources.base_size))
                }
                CursorFallbackState::Uninitialized => None,
            }
        };

        let Some(shm) = self.shm.as_ref().cloned() else {
            self.disable_cursor_fallback(CursorFallbackError::Message(
                "wl_shm is unavailable for the legacy cursor path".to_string(),
            ));
            return false;
        };
        let (theme_name, base_size) = existing_spec.unwrap_or_else(cursor_theme_spec);
        let size = cursor_theme_size(base_size, scale_bucket);
        let mut theme = match catch_unwind(AssertUnwindSafe(|| {
            CursorTheme::load_from_name(connection, shm, &theme_name, size)
        })) {
            Ok(Ok(theme)) => theme,
            Ok(Err(error)) => {
                self.disable_cursor_fallback(CursorFallbackError::Message(format!(
                    "failed to create cursor theme: {error}"
                )));
                return false;
            }
            Err(_) => {
                self.disable_cursor_fallback(CursorFallbackError::Panicked);
                return false;
            }
        };
        let images = match CursorFallbackImages::load(&mut theme) {
            Ok(images) => images,
            Err(error) => {
                self.disable_cursor_fallback(error);
                return false;
            }
        };

        let mut fallback = self.cursor_fallback.borrow_mut();
        match &mut *fallback {
            CursorFallbackState::Ready(resources) => {
                resources.theme = theme;
                resources.images = images;
                resources.scale_bucket = scale_bucket;
                resources.last_applied = None;
            }
            CursorFallbackState::Uninitialized => {
                let Some(compositor) = self.compositor.as_ref() else {
                    drop(fallback);
                    self.disable_cursor_fallback(CursorFallbackError::Message(
                        "Wayland compositor is unavailable for the legacy cursor surface"
                            .to_string(),
                    ));
                    return false;
                };
                *fallback = CursorFallbackState::Ready(Box::new(CursorFallbackResources {
                    surface: compositor.create_surface(qh, ()),
                    theme,
                    images,
                    theme_name,
                    base_size,
                    scale_bucket,
                    last_applied: None,
                }));
            }
            CursorFallbackState::Disabled => return false,
        }
        true
    }

    fn clear_cursor_fallback_applied(&self) {
        if let CursorFallbackState::Ready(resources) = &mut *self.cursor_fallback.borrow_mut() {
            resources.last_applied = None;
        }
    }

    fn disable_cursor_fallback(&self, error: CursorFallbackError) {
        let previous = {
            let mut fallback = self.cursor_fallback.borrow_mut();
            if matches!(&*fallback, CursorFallbackState::Disabled) {
                return;
            }
            std::mem::replace(&mut *fallback, CursorFallbackState::Disabled)
        };
        if let CursorFallbackState::Ready(resources) = previous {
            resources.surface.destroy();
        }
        match error {
            CursorFallbackError::Message(message) => {
                eprintln!("Wayland cursor fallback disabled: {message}");
            }
            CursorFallbackError::Panicked => {
                eprintln!("Wayland cursor fallback disabled after cursor-theme failure");
            }
        }
    }

    fn release_cursor_fallback(&self) {
        let previous = {
            let mut fallback = self.cursor_fallback.borrow_mut();
            std::mem::replace(&mut *fallback, CursorFallbackState::Disabled)
        };
        if let CursorFallbackState::Ready(resources) = previous {
            resources.surface.destroy();
        }
    }

    fn release_seat(&mut self) {
        self.activation_serial = None;
        self.remove_keyboard();
        self.remove_pointer();
        if let Some(text_input) = self.text_input.take() {
            text_input.destroy();
        }
        if let Some(seat) = self.seat.take()
            && seat.version() >= 5
        {
            seat.release();
        }
        self.seat_global_name = None;
    }

    fn queue_key_event(&mut self, evdev_key: u32, state: KeyState, repeat: bool, now: Instant) {
        let physical_key = physical_key_from_evdev(evdev_key);
        self.push_input(WaylandInputEvent::Key(KeyInput {
            state,
            physical_key,
        }));
        if should_emit_xkb_text(
            state,
            physical_key,
            self.modifiers,
            self.text_input_enabled,
            repeat,
            self.ime.preedit_active(),
        ) && let Some(text) = self.xkb.text_for_key(evdev_key)
            && !text.is_empty()
        {
            self.push_input(WaylandInputEvent::Text(text));
        }
        if repeat {
            return;
        }
        match state {
            KeyState::Pressed if self.xkb.key_repeats(evdev_key) => {
                self.repeat.start(evdev_key, now)
            }
            KeyState::Pressed => {}
            KeyState::Released => self.repeat.stop_key(evdev_key),
        }
    }

    fn repeat_deadline(&self) -> Option<Instant> {
        self.repeat.deadline()
    }

    fn process_repeat_deadline(&mut self, now: Instant) {
        if !self.keyboard_focused {
            self.repeat.stop();
            return;
        }
        if let Some(key) = self.repeat.take_due(now) {
            self.queue_key_event(key, KeyState::Pressed, true, now);
        }
    }

    fn set_scale_factor(&mut self, scale_factor: f32) {
        if !scale_factor.is_finite()
            || scale_factor <= 0.0
            || (self.scale_factor - scale_factor).abs() <= f32::EPSILON
        {
            return;
        }
        self.scale_factor = scale_factor;
        self.apply_surface_geometry();
        let metrics = self.metrics();
        self.pending_scale_change = Some(metrics);
        if self.pending_configure.is_some() {
            self.pending_configure = Some(metrics);
        }
    }

    fn set_scale_factor_and_cursor(
        &mut self,
        connection: &Connection,
        qh: &QueueHandle<Self>,
        scale_factor: f32,
    ) {
        let old_bucket = cursor_scale_bucket(self.scale_factor);
        self.set_scale_factor(scale_factor);
        if old_bucket != cursor_scale_bucket(self.scale_factor)
            && matches!(
                self.cursor_fallback_status(),
                CursorFallbackStatus::Ready { .. }
            )
        {
            self.prepare_cursor_fallback_if_needed(connection, qh);
            self.apply_cursor();
        }
    }

    fn apply_configure(&mut self) {
        if let Some((width, height)) = self.pending_toplevel_size.take() {
            self.logical_width = configured_dimension(self.logical_width, width);
            self.logical_height = configured_dimension(self.logical_height, height);
        }
        self.configured = true;
        self.apply_surface_geometry();
        let metrics = self.metrics();
        self.pending_configure = Some(metrics);
        if self.pending_scale_change.is_some() {
            self.pending_scale_change = Some(metrics);
        }
    }

    fn take_events(&mut self) -> WaylandRuntimeEvents {
        WaylandRuntimeEvents {
            configured: self.pending_configure.take(),
            scale_changed: self.pending_scale_change.take(),
            close_requested: std::mem::take(&mut self.close_requested),
            input: self.pending_input.drain(..).collect(),
        }
    }

    fn clear_pending_events(&mut self) {
        self.pending_configure = None;
        self.pending_scale_change = None;
        self.close_requested = false;
        self.pending_input.clear();
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for WaylandState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => match interface.as_str() {
                "wl_compositor" if state.compositor.is_none() => {
                    state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                        name,
                        version.min(WAYLAND_COMPOSITOR_MAX_VERSION),
                        qh,
                        (),
                    ));
                }
                "wl_shm" if state.shm.is_none() => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(
                        name,
                        version.min(WAYLAND_SHM_VERSION),
                        qh,
                        (),
                    ));
                }
                "xdg_wm_base" if state.wm_base.is_none() => {
                    state.wm_base = Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(
                        name,
                        version.min(6),
                        qh,
                        (),
                    ));
                }
                "xdg_activation_v1" if state.activation.is_none() => {
                    state.activation_global_name = Some(name);
                    state.activation =
                        Some(registry.bind::<xdg_activation_v1::XdgActivationV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ));
                }
                "zxdg_decoration_manager_v1" if state.decoration_manager.is_none() => {
                    state.decoration_manager = Some(
                        registry.bind::<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ),
                    );
                }
                "wp_fractional_scale_manager_v1" if state.fractional_scale_manager.is_none() => {
                    state.fractional_scale_manager = Some(
                        registry
                            .bind::<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1, _, _>(
                                name,
                                version.min(1),
                                qh,
                                (),
                            ),
                    );
                }
                "wp_viewporter" if state.viewporter.is_none() => {
                    state.viewporter = Some(registry.bind::<wp_viewporter::WpViewporter, _, _>(
                        name,
                        version.min(1),
                        qh,
                        (),
                    ));
                }
                "wl_seat" if state.seat.is_none() => {
                    state.seat_global_name = Some(name);
                    state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(
                        name,
                        version.min(WAYLAND_SEAT_MAX_VERSION),
                        qh,
                        (),
                    ));
                }
                "zwp_text_input_manager_v3" if state.text_input_manager.is_none() => {
                    state.text_input_manager = Some(
                        registry.bind::<zwp_text_input_manager_v3::ZwpTextInputManagerV3, _, _>(
                            name,
                            version.min(1),
                            qh,
                            (),
                        ),
                    );
                    state.ensure_text_input(qh);
                }
                "wp_cursor_shape_manager_v1" if state.cursor_shape_manager.is_none() => {
                    state.cursor_shape_manager = Some(
                        registry.bind::<wp_cursor_shape_manager_v1::WpCursorShapeManagerV1, _, _>(
                            name,
                            version.min(2),
                            qh,
                            (),
                        ),
                    );
                    state.ensure_cursor_shape(qh);
                    state.apply_cursor();
                }
                _ => {}
            },
            wl_registry::Event::GlobalRemove { name } if state.seat_global_name == Some(name) => {
                state.release_seat();
            }
            wl_registry::Event::GlobalRemove { name }
                if state.activation_global_name == Some(name) =>
            {
                state.release_activation();
            }
            _ => {}
        }
    }
}

delegate_noop!(WaylandState: ignore wl_compositor::WlCompositor);
delegate_noop!(WaylandState: ignore wl_shm::WlShm);
delegate_noop!(WaylandState: ignore xdg_activation_v1::XdgActivationV1);
delegate_noop!(WaylandState: ignore zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);
delegate_noop!(WaylandState: ignore zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1);
delegate_noop!(WaylandState: ignore wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(WaylandState: ignore wp_viewporter::WpViewporter);
delegate_noop!(WaylandState: ignore wp_viewport::WpViewport);

delegate_noop!(WaylandState: ignore zwp_text_input_manager_v3::ZwpTextInputManagerV3);
delegate_noop!(WaylandState: ignore wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(WaylandState: ignore wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);

impl Dispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities { capabilities } = event else {
            return;
        };
        let WEnum::Value(capabilities) = capabilities else {
            return;
        };
        if capabilities.contains(wl_seat::Capability::Keyboard) {
            state.ensure_keyboard(qh);
        } else {
            state.remove_keyboard();
        }
        if capabilities.contains(wl_seat::Capability::Pointer) {
            state.ensure_pointer(connection, qh);
        } else {
            state.remove_pointer();
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                if !matches!(format, WEnum::Value(wl_keyboard::KeymapFormat::XkbV1)) {
                    return;
                }
                if let Err(error) = state.xkb.set_keymap(fd, size) {
                    eprintln!("Ronsole: rejected Wayland XKB keymap: {error}");
                }
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } if state
                .surface
                .as_ref()
                .is_some_and(|own| own.id() == surface.id()) =>
            {
                state.activation_serial = Some(serial);
                state.set_keyboard_focus(true);
            }
            wl_keyboard::Event::Leave {
                serial, surface, ..
            } if state
                .surface
                .as_ref()
                .is_some_and(|own| own.id() == surface.id()) =>
            {
                state.set_keyboard_focus(false);
                state.activation_serial = Some(serial);
            }
            wl_keyboard::Event::Key {
                serial,
                key,
                state: key_state,
                ..
            } if state.keyboard_focused => {
                state.activation_serial = Some(serial);
                let key_state = match key_state {
                    WEnum::Value(wl_keyboard::KeyState::Pressed) => KeyState::Pressed,
                    WEnum::Value(wl_keyboard::KeyState::Released) => KeyState::Released,
                    _ => return,
                };
                state.queue_key_event(key, key_state, false, Instant::now());
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                let modifiers =
                    state
                        .xkb
                        .update_modifiers(mods_depressed, mods_latched, mods_locked, group);
                state.modifiers = modifiers;
                if state.keyboard_focused {
                    state.push_input(WaylandInputEvent::Modifiers(modifiers));
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => {
                state
                    .repeat
                    .set_config(RepeatConfig::from_wayland(rate, delay));
            }
            _ => {}
        }
    }
}

impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for WaylandState {
    fn event(
        state: &mut Self,
        text_input: &zwp_text_input_v3::ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwp_text_input_v3::Event::Enter { surface }
                if state
                    .surface
                    .as_ref()
                    .is_some_and(|own| own.id() == surface.id()) =>
            {
                text_input.enable();
                text_input.set_content_type(
                    zwp_text_input_v3::ContentHint::None,
                    zwp_text_input_v3::ContentPurpose::Terminal,
                );
                text_input.commit();
                state.text_input_enabled = true;
                state.ime.clear();
            }
            zwp_text_input_v3::Event::Leave { surface }
                if state
                    .surface
                    .as_ref()
                    .is_some_and(|own| own.id() == surface.id()) =>
            {
                state.disable_text_input();
            }
            zwp_text_input_v3::Event::PreeditString { text, .. } if state.text_input_enabled => {
                state.ime.preedit(text);
            }
            zwp_text_input_v3::Event::CommitString { text } if state.text_input_enabled => {
                state.ime.commit_string(text);
            }
            zwp_text_input_v3::Event::Done { .. } if state.text_input_enabled => {
                if let Some(text) = state.ime.done() {
                    state.push_input(WaylandInputEvent::ImeCommit(text));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for WaylandState {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } if state
                .surface
                .as_ref()
                .is_some_and(|own| own.id() == surface.id()) =>
            {
                state.pointer_enter_serial = Some(serial);
                state.apply_cursor();
                state.push_pointer_frame(WaylandInputEvent::PointerMotion(pointer_position(
                    surface_x,
                    surface_y,
                    state.scale_factor,
                )));
            }
            wl_pointer::Event::Leave { surface, .. }
                if state
                    .surface
                    .as_ref()
                    .is_some_and(|own| own.id() == surface.id()) =>
            {
                state.pointer_enter_serial = None;
                state.clear_cursor_fallback_applied();
                state.push_pointer_frame(WaylandInputEvent::PointerLeave);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => state.push_pointer_frame(WaylandInputEvent::PointerMotion(pointer_position(
                surface_x,
                surface_y,
                state.scale_factor,
            ))),
            wl_pointer::Event::Button {
                serial,
                button,
                state: button_state,
                ..
            } => {
                if state.keyboard_focused {
                    state.activation_serial = Some(serial);
                }
                let button_state = match button_state {
                    WEnum::Value(wl_pointer::ButtonState::Pressed) => KeyState::Pressed,
                    WEnum::Value(wl_pointer::ButtonState::Released) => KeyState::Released,
                    _ => return,
                };
                state.push_pointer_frame(WaylandInputEvent::PointerButton(
                    button_state,
                    pointer_button_from_linux(button),
                ));
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let WEnum::Value(axis) = axis else { return };
                match axis {
                    wl_pointer::Axis::HorizontalScroll => {
                        state.pointer_axes.set_absolute(true, value)
                    }
                    wl_pointer::Axis::VerticalScroll => {
                        state.pointer_axes.set_absolute(false, value)
                    }
                    _ => {}
                }
            }
            wl_pointer::Event::AxisSource { axis_source } => {
                let source = match axis_source {
                    WEnum::Value(wl_pointer::AxisSource::Wheel) => PointerAxisSource::Wheel,
                    WEnum::Value(wl_pointer::AxisSource::Finger) => PointerAxisSource::Finger,
                    WEnum::Value(wl_pointer::AxisSource::Continuous) => {
                        PointerAxisSource::Continuous
                    }
                    WEnum::Value(wl_pointer::AxisSource::WheelTilt) => PointerAxisSource::WheelTilt,
                    _ => PointerAxisSource::Unknown,
                };
                state.pointer_axes.set_source(source);
            }
            wl_pointer::Event::AxisDiscrete { axis, discrete } => {
                let WEnum::Value(axis) = axis else { return };
                state
                    .pointer_axes
                    .add_discrete(matches!(axis, wl_pointer::Axis::HorizontalScroll), discrete);
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                let WEnum::Value(axis) = axis else { return };
                state
                    .pointer_axes
                    .add_value120(matches!(axis, wl_pointer::Axis::HorizontalScroll), value120);
            }
            wl_pointer::Event::AxisStop { .. } => {}
            wl_pointer::Event::Frame => state.flush_pointer_frame(),
            _ => {}
        }
        if pointer.version() < 5 {
            state.flush_pointer_frame();
        }
    }
}

impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        token_proxy: &xdg_activation_token_v1::XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let is_pending = state
            .pending_activation_token
            .as_ref()
            .is_some_and(|pending| pending.id() == token_proxy.id());
        if !is_pending {
            token_proxy.destroy();
            return;
        }
        state.pending_activation_token.take();
        if let xdg_activation_token_v1::Event::Done { token } = event
            && !state.keyboard_focused
            && let (Some(activation), Some(surface)) =
                (state.activation.as_ref(), state.surface.as_ref())
        {
            activation.activate(token, surface);
        }
        token_proxy.destroy();
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                state.pending_toplevel_size = Some((width, height));
            }
            xdg_toplevel::Event::Close => state.close_requested = true,
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for WaylandState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            state.apply_configure();
        }
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for WaylandState {
    fn event(
        state: &mut Self,
        surface: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if !state
            .surface
            .as_ref()
            .is_some_and(|main_surface| main_surface.id() == surface.id())
        {
            return;
        }
        if state.fractional_scaling_active() {
            return;
        }
        if let wl_surface::Event::PreferredBufferScale { factor } = event
            && factor > 0
        {
            state.set_scale_factor_and_cursor(connection, qh, factor as f32);
        }
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        _: &(),
        connection: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            state.set_scale_factor_and_cursor(
                connection,
                qh,
                scale as f32 / FRACTIONAL_SCALE_DENOMINATOR,
            );
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for WaylandState {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        _: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.frame.callback_done();
    }
}

struct DirectWaylandBackend {
    connection: Connection,
    event_queue: EventQueue<WaylandState>,
    qh: QueueHandle<WaylandState>,
    state: WaylandState,
}

impl DirectWaylandBackend {
    fn connect(logical_width: u32, logical_height: u32) -> Result<Self, String> {
        let connection = Connection::connect_to_env()
            .map_err(|error| format!("failed to connect to Wayland compositor: {error}"))?;
        let mut event_queue = connection.new_event_queue();
        let qh = event_queue.handle();
        let mut state = WaylandState::new(logical_width, logical_height)?;
        state.registry = Some(connection.display().get_registry(&qh, ()));
        event_queue
            .roundtrip(&mut state)
            .map_err(|error| format!("Wayland registry roundtrip failed: {error}"))?;
        state.create_window(&qh)?;

        while !state.configured && !state.close_requested {
            event_queue.blocking_dispatch(&mut state).map_err(|error| {
                format!("waiting for initial Wayland configure failed: {error}")
            })?;
        }
        if state.close_requested {
            return Err("Wayland toplevel closed before initial configure".to_string());
        }
        state.prepare_cursor_fallback_if_needed(&connection, &qh);
        state.clear_pending_events();

        Ok(Self {
            connection,
            event_queue,
            qh,
            state,
        })
    }

    fn metrics(&self) -> WaylandWindowMetrics {
        self.state.metrics()
    }

    fn raw_display_handle(&self) -> Result<RawDisplayHandle, String> {
        let pointer = NonNull::new(self.connection.backend().display_ptr().cast())
            .ok_or_else(|| "Wayland display pointer is null".to_string())?;
        Ok(RawDisplayHandle::Wayland(WaylandDisplayHandle::new(
            pointer,
        )))
    }

    fn raw_window_handle(&self) -> Result<RawWindowHandle, String> {
        let surface = self
            .state
            .surface
            .as_ref()
            .ok_or_else(|| "Wayland surface is unavailable".to_string())?;
        raw_wayland_window(surface)
    }

    fn request_frame(&self) {
        self.state.frame.request();
        self.arm_frame_callback();
    }

    fn arm_frame_callback(&self) {
        if !self.state.frame.should_arm_callback() {
            return;
        }
        let Some(surface) = self.state.surface.as_ref() else {
            return;
        };
        surface.frame(&self.qh, ());
        self.state.frame.mark_callback_outstanding();
        surface.commit();
        let _ = self.connection.flush();
    }

    fn take_frame_ready(&self) -> bool {
        self.state.frame.take_ready()
    }

    fn frame_ready_requested(&self) -> bool {
        self.state.frame.ready_requested()
    }

    fn mark_presented(&self) {
        self.state.frame.mark_presented();
        self.arm_frame_callback();
    }

    fn take_events(&mut self) -> WaylandRuntimeEvents {
        self.state.take_events()
    }

    fn has_dispatch_activity(&self) -> bool {
        self.state.pending_configure.is_some()
            || self.state.pending_scale_change.is_some()
            || self.state.close_requested
            || !self.state.pending_input.is_empty()
            || self.state.frame.ready_requested()
    }

    fn input_deadline(&self) -> Option<Instant> {
        self.state.repeat_deadline()
    }

    fn process_input_deadline(&mut self, now: Instant) {
        self.state.process_repeat_deadline(now);
    }

    fn set_cursor_kind(&self, kind: CursorKind) {
        self.state.set_cursor_kind(kind);
    }

    fn activate_existing_window(&mut self, activation_token: Option<String>) {
        self.state
            .activate_existing_window(&self.qh, activation_token);
        let _ = self.connection.flush();
    }

    fn dispatch_pending(&mut self) -> Result<usize, String> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|error| format!("Wayland event dispatch failed: {error}"))
    }

    fn flush_would_block(&self) -> Result<bool, String> {
        match self.connection.flush() {
            Ok(()) => Ok(false),
            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Ok(true)
            }
            Err(error) => Err(format!("Wayland flush failed: {error}")),
        }
    }

    fn poll(&mut self, wake: &WakeHandle, timeout_ms: i32) -> Result<WaylandPollOutcome, String> {
        loop {
            let _ = self.dispatch_pending()?;
            if self.has_dispatch_activity() {
                return Ok(WaylandPollOutcome::default());
            }

            let want_write = self.flush_would_block()?;
            let Some(read_guard) = self.connection.prepare_read() else {
                continue;
            };

            let mut wayland_events = libc::POLLIN;
            if want_write {
                wayland_events |= libc::POLLOUT;
            }
            let mut fds = [
                libc::pollfd {
                    fd: self.connection.as_fd().as_raw_fd(),
                    events: wayland_events,
                    revents: 0,
                },
                libc::pollfd {
                    fd: wake.as_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];

            let poll_result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, timeout_ms) };
            if poll_result < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::Interrupted {
                    drop(read_guard);
                    return Ok(WaylandPollOutcome::default());
                }
                return Err(format!("Wayland event poll failed: {error}"));
            }

            if poll_result == 0 {
                drop(read_guard);
                return Ok(WaylandPollOutcome {
                    woke: false,
                    timed_out: true,
                });
            }

            let fatal = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;
            if fds[0].revents & fatal != 0 {
                drop(read_guard);
                return Err("Wayland connection became unavailable".to_string());
            }
            if fds[1].revents & fatal != 0 {
                drop(read_guard);
                return Err("wake eventfd became unavailable".to_string());
            }

            if fds[0].revents & libc::POLLIN != 0 {
                match read_guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(error))
                        if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(format!("reading Wayland events failed: {error}")),
                }
            } else {
                drop(read_guard);
            }

            if fds[0].revents & libc::POLLOUT != 0 {
                let _ = self.flush_would_block()?;
            }

            let woke = if fds[1].revents & libc::POLLIN != 0 {
                wake.drain()
                    .map_err(|error| format!("draining wake eventfd failed: {error}"))?;
                true
            } else {
                false
            };

            let _ = self.dispatch_pending()?;
            return Ok(WaylandPollOutcome {
                woke,
                timed_out: false,
            });
        }
    }
}

impl Drop for DirectWaylandBackend {
    fn drop(&mut self) {
        self.state.repeat.stop();
        self.state.disable_text_input();
        self.state.release_activation();
        if let Some(text_input) = self.state.text_input.take() {
            text_input.destroy();
        }
        if let Some(cursor_shape_device) = self.state.cursor_shape_device.take() {
            cursor_shape_device.destroy();
        }
        if let Some(keyboard) = self.state.keyboard.take()
            && keyboard.version() >= 3
        {
            keyboard.release();
        }
        if let Some(pointer) = self.state.pointer.take()
            && pointer.version() >= 3
        {
            pointer.release();
        }
        self.state.release_cursor_fallback();
        if let Some(seat) = self.state.seat.take()
            && seat.version() >= 5
        {
            seat.release();
        }
        if let Some(fractional_scale) = self.state.fractional_scale.take() {
            fractional_scale.destroy();
        }
        if let Some(viewport) = self.state.viewport.take() {
            viewport.destroy();
        }
        if let Some(decoration) = self.state.decoration.take() {
            decoration.destroy();
        }
        if let Some(toplevel) = self.state.toplevel.take() {
            toplevel.destroy();
        }
        if let Some(xdg_surface) = self.state.xdg_surface.take() {
            xdg_surface.destroy();
        }
        if let Some(surface) = self.state.surface.take() {
            surface.destroy();
        }
        if let Some(decoration_manager) = self.state.decoration_manager.take() {
            decoration_manager.destroy();
        }
        if let Some(fractional_scale_manager) = self.state.fractional_scale_manager.take() {
            fractional_scale_manager.destroy();
        }
        if let Some(viewporter) = self.state.viewporter.take() {
            viewporter.destroy();
        }
        if let Some(cursor_shape_manager) = self.state.cursor_shape_manager.take() {
            cursor_shape_manager.destroy();
        }
        if let Some(text_input_manager) = self.state.text_input_manager.take() {
            text_input_manager.destroy();
        }
        if let Some(wm_base) = self.state.wm_base.take() {
            wm_base.destroy();
        }
        let _ = self.connection.flush();
    }
}

pub(crate) struct TerminalRenderParams<'a> {
    pub terminals: &'a [crate::terminal::Terminal],
    pub active_terminal: usize,
    pub search: &'a mut crate::search::TerminalSearchState,
    pub focused: bool,
    pub tab_scroll_x: f32,
    pub drag: Option<&'a crate::tabs::TabDragState>,
    pub pointer_x: f32,
    pub pointer_y: f32,
    pub settings_progress: f32,
    pub settings_tab: crate::renderer::SettingsTab,
    pub settings_font_value: &'a str,
    pub settings_scroll_value: &'a str,
    pub settings_background_input: &'a mut crate::single_line_input::SingleLineInput,
    pub settings_background_editing: bool,
}

pub struct WindowRuntime {
    renderer: Renderer,
    config: Config,
    context: PossiblyCurrentContext,
    surface: Surface<WindowSurface>,
    wayland: Box<DirectWaylandBackend>,
    wake: WakeHandle,
    requested_plan: GlContextPlan,
    requested_priority: GlContextPriorityRequest,
    swap_interval_applied: bool,
}

impl WindowRuntime {
    pub(crate) fn bootstrap_wayland(
        logical_width: f64,
        logical_height: f64,
        terminal_font_size: f32,
        terminal_background: crate::config::RgbColor,
    ) -> Result<Self, String> {
        let wayland = DirectWaylandBackend::connect(
            logical_dimension(logical_width),
            logical_dimension(logical_height),
        )?;
        let metrics = wayland.metrics();
        if !surface_renderable(
            wayland.state.configured,
            metrics.physical_width,
            metrics.physical_height,
        ) {
            return Err("Wayland surface is not renderable after initial configure".to_string());
        }

        let raw_display = wayland.raw_display_handle()?;
        let gl_display = unsafe { GlutinDisplay::new(raw_display, DisplayApiPreference::Egl) }
            .map_err(|error| format!("EGL display creation failed: {error}"))?;
        let gl_config = choose_direct_gl_config(&gl_display)?;
        let raw_window_handle = wayland.raw_window_handle()?;
        let (not_current_context, requested_plan, requested_priority) =
            create_not_current_context(&gl_config, raw_window_handle)?;
        let (surface, context, swap_interval_applied) = create_surface_and_context(
            &gl_config,
            raw_window_handle,
            metrics.physical_width,
            metrics.physical_height,
            not_current_context,
        )?;
        let mut renderer = Renderer::new(
            create_glow_context(&gl_config),
            metrics.scale_factor,
            terminal_font_size,
            terminal_background,
        )
        .map_err(|error| format!("renderer initialization failed: {error}"))?;
        renderer.resize(metrics.physical_width, metrics.physical_height);
        let wake =
            WakeHandle::new().map_err(|error| format!("eventfd creation failed: {error}"))?;
        let runtime = Self {
            renderer,
            config: gl_config,
            context,
            surface,
            wayland: Box::new(wayland),
            wake,
            requested_plan,
            requested_priority,
            swap_interval_applied,
        };
        trim_allocator_after_gl_bootstrap();
        Ok(runtime)
    }

    pub(crate) fn wayland_metrics(&self) -> WaylandWindowMetrics {
        self.wayland.metrics()
    }

    pub(crate) fn wake_handle(&self) -> WakeHandle {
        self.wake.clone()
    }

    pub(crate) fn request_frame(&self) {
        self.wayland.request_frame();
    }

    pub(crate) fn acknowledge_wake(&self) {
        let _ = self.wake.drain();
    }

    pub(crate) fn take_wayland_frame_ready(&self) -> bool {
        self.wayland.take_frame_ready()
    }

    pub(crate) fn wayland_frame_ready_requested(&self) -> bool {
        self.wayland.frame_ready_requested()
    }

    pub(crate) fn poll_wayland(&mut self, timeout_ms: i32) -> Result<WaylandPollOutcome, String> {
        let wake = self.wake.clone();
        self.wayland.poll(&wake, timeout_ms)
    }

    pub(crate) fn take_wayland_events(&mut self) -> WaylandRuntimeEvents {
        self.wayland.take_events()
    }

    pub(crate) fn wayland_input_deadline(&self) -> Option<Instant> {
        self.wayland.input_deadline()
    }

    pub(crate) fn process_wayland_input_deadline(&mut self, now: Instant) {
        self.wayland.process_input_deadline(now);
    }

    pub fn activate_existing_window(&mut self, activation_token: Option<String>) {
        self.wayland.activate_existing_window(activation_token);
    }

    pub fn update_scale_factor(&mut self, scale_factor: f32) {
        self.renderer.update_scale_factor(scale_factor);
    }

    pub(crate) fn set_terminal_font_size(&mut self, logical_size: f32) -> bool {
        self.renderer.set_terminal_font_size(logical_size)
    }

    pub(crate) fn set_terminal_background(&mut self, background: crate::config::RgbColor) -> bool {
        self.renderer.set_terminal_background(background)
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        let Some(width) = NonZeroU32::new(width) else {
            return;
        };
        let Some(height) = NonZeroU32::new(height) else {
            return;
        };
        self.surface.resize(&self.context, width, height);
        self.renderer.resize(width.get(), height.get());
    }

    pub fn render_terminal_and_present(
        &mut self,
        params: TerminalRenderParams<'_>,
    ) -> Result<crate::renderer::TerminalUiLayout, String> {
        let layout = self.renderer.render_terminal_app(
            params.terminals,
            params.active_terminal,
            params.search,
            params.focused,
            params.tab_scroll_x,
            params.drag,
            params.pointer_x,
            params.pointer_y,
            params.settings_progress,
            params.settings_tab,
            params.settings_font_value,
            params.settings_scroll_value,
            params.settings_background_input,
            params.settings_background_editing,
        );
        self.surface
            .swap_buffers(&self.context)
            .map_err(|error| format!("swap buffers failed: {error}"))?;
        self.wayland.mark_presented();
        Ok(layout)
    }

    pub fn terminal_tab_hit_test(&self, x: f32, y: f32) -> crate::renderer::TerminalTabHit {
        self.renderer.terminal_tab_hit_test(x, y)
    }

    pub(crate) fn settings_hit_test(
        &self,
        progress: f32,
        active_tab: crate::renderer::SettingsTab,
        x: f32,
        y: f32,
    ) -> crate::renderer::SettingsHit {
        self.renderer.settings_hit_test(progress, active_tab, x, y)
    }

    pub(crate) fn settings_background_cursor_from_x(
        &mut self,
        progress: f32,
        active_tab: crate::renderer::SettingsTab,
        text: &str,
        x: f32,
        scroll_x: f32,
    ) -> Option<usize> {
        self.renderer
            .settings_background_cursor_from_x(progress, active_tab, text, x, scroll_x)
    }

    pub(crate) fn set_cursor_kind(&self, kind: CursorKind) {
        self.wayland.set_cursor_kind(kind);
    }

    pub fn terminal_tab_strip_layout(&self) -> crate::renderer::TerminalTabStripLayout {
        self.renderer.terminal_tab_strip_layout()
    }

    pub fn terminal_tab_drag_destination(&self, drag: &crate::tabs::TabDragState) -> Option<usize> {
        self.renderer.terminal_tab_drag_destination(drag)
    }

    pub fn terminal_tab_reveal_target(
        &self,
        active_idx: usize,
        reveal_tail: bool,
        current_target: f32,
    ) -> f32 {
        self.renderer
            .terminal_tab_reveal_target(active_idx, reveal_tail, current_target)
    }

    pub fn terminal_tab_animation_active(&self) -> bool {
        self.renderer.terminal_tab_animation_active()
    }

    pub fn scale_factor(&self) -> f32 {
        self.renderer.scale_factor()
    }

    pub fn terminal_search_cursor_from_x(&mut self, text: &str, x: f32, scroll_x: f32) -> usize {
        self.renderer
            .terminal_cursor_from_input_x(text, x, scroll_x)
    }

    fn physical_size(&self) -> (u32, u32) {
        let metrics = self.wayland.metrics();
        (metrics.physical_width, metrics.physical_height)
    }

    pub fn diagnostics_report(&self) -> String {
        let (width, height) = self.physical_size();
        let gl = self.renderer.graphics_diagnostics();
        format!(
            "Context request: {} / {} priority\nActual context priority: {}\nOpenGL: {}\nGLSL: {}\nGPU: {} / {}\nFramebuffer: hardware={} samples={} depth={} stencil={} alpha={} srgb={} transparent={:?}\nFramebuffer size: {}x{}\nScale factor: {:.3}\nSwap interval: Wait(1), applied={}",
            self.requested_plan.label(),
            self.requested_priority.label(),
            priority_label(self.context.priority()),
            gl.version,
            gl.shading_language,
            gl.vendor,
            gl.renderer,
            self.config.hardware_accelerated(),
            self.config.num_samples(),
            self.config.depth_size(),
            self.config.stencil_size(),
            self.config.alpha_size(),
            self.config.srgb_capable(),
            self.config.supports_transparency(),
            width,
            height,
            self.renderer.scale_factor(),
            self.swap_interval_applied,
        )
    }
}

fn priority_label(priority: Priority) -> &'static str {
    match priority {
        Priority::Low => "Low",
        Priority::Medium => "Medium",
        Priority::High => "High",
        Priority::Realtime => "Realtime",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framebuffer_config_rank_prefers_hardware_then_minimum_samples() {
        assert!(framebuffer_config_rank(true, 0) > framebuffer_config_rank(true, 8));
        assert!(framebuffer_config_rank(true, 8) > framebuffer_config_rank(false, 0));
        assert!(framebuffer_config_rank(false, 0) > framebuffer_config_rank(false, 8));
    }

    #[test]
    fn linux_priority_fallback_stays_within_each_graphics_plan() {
        let attempts = gl_context_attempts().take(6).collect::<Vec<_>>();
        assert_eq!(
            attempts,
            [
                (
                    GlContextPlan::Desktop { major: 4, minor: 1 },
                    GlContextPriorityRequest::High,
                ),
                (
                    GlContextPlan::Desktop { major: 4, minor: 1 },
                    GlContextPriorityRequest::Default,
                ),
                (
                    GlContextPlan::Desktop { major: 3, minor: 3 },
                    GlContextPriorityRequest::High,
                ),
                (
                    GlContextPlan::Desktop { major: 3, minor: 3 },
                    GlContextPriorityRequest::Default,
                ),
                (
                    GlContextPlan::Gles { major: 3, minor: 0 },
                    GlContextPriorityRequest::High,
                ),
                (
                    GlContextPlan::Gles { major: 3, minor: 0 },
                    GlContextPriorityRequest::Default,
                ),
            ]
        );
    }

    #[test]
    fn swap_policy_is_blocking_wait_one() {
        assert_eq!(
            blocking_swap_interval(),
            SwapInterval::Wait(NonZeroU32::MIN)
        );
    }

    #[test]
    fn xdg_activation_plan_uses_one_shot_tokens_and_coalesces_fallback_requests() {
        assert_eq!(
            activation_request_plan(false, true, false, false),
            ActivationRequestPlan::Unavailable
        );
        assert_eq!(
            activation_request_plan(true, true, true, false),
            ActivationRequestPlan::ActivateSuppliedToken
        );
        assert_eq!(
            activation_request_plan(true, false, false, true),
            ActivationRequestPlan::RequestToken
        );
        assert_eq!(
            activation_request_plan(true, false, true, true),
            ActivationRequestPlan::AwaitPendingToken
        );
        assert_eq!(
            activation_request_plan(true, false, false, false),
            ActivationRequestPlan::MissingFallbackContext
        );
    }

    #[test]
    fn context_plan_order_matches_rriter_linux_fallbacks() {
        assert_eq!(
            gl_context_plans(),
            [
                GlContextPlan::Desktop { major: 4, minor: 1 },
                GlContextPlan::Desktop { major: 3, minor: 3 },
                GlContextPlan::Gles { major: 3, minor: 0 },
            ]
        );
    }

    #[test]
    fn configure_zero_dimensions_keep_last_logical_size() {
        assert_eq!(configured_dimension(1100, 0), 1100);
        assert_eq!(configured_dimension(720, 0), 720);
        assert_eq!(configured_dimension(1100, 900), 900);
    }

    #[test]
    fn configure_and_scale_events_coalesce_to_latest_metrics_in_either_order() {
        let mut configure_then_scale = WaylandState::new(800, 600).expect("XKB must initialize");
        configure_then_scale.pending_toplevel_size = Some((1000, 700));
        configure_then_scale.apply_configure();
        configure_then_scale.set_scale_factor(1.5);
        let events = configure_then_scale.take_events();
        let expected = WaylandWindowMetrics::new(1000, 700, 1.5);
        assert_eq!(events.configured, Some(expected));
        assert_eq!(events.scale_changed, Some(expected));

        let mut scale_then_configure = WaylandState::new(800, 600).expect("XKB must initialize");
        scale_then_configure.set_scale_factor(1.25);
        scale_then_configure.pending_toplevel_size = Some((1000, 700));
        scale_then_configure.apply_configure();
        let events = scale_then_configure.take_events();
        let expected = WaylandWindowMetrics::new(1000, 700, 1.25);
        assert_eq!(events.configured, Some(expected));
        assert_eq!(events.scale_changed, Some(expected));
    }

    #[test]
    fn keyboard_focus_loss_clears_transient_input_and_repeat_state() {
        let mut state = WaylandState::new(800, 600).expect("XKB must initialize");
        state.keyboard_focused = true;
        state.modifiers = Modifiers::CONTROL | Modifiers::SHIFT;
        state.text_input_enabled = true;
        state.ime.preedit(Some("compose".to_string()));
        state.repeat.start(30, Instant::now());

        state.set_keyboard_focus(false);

        assert!(!state.keyboard_focused);
        assert!(state.modifiers.is_empty());
        assert_eq!(state.repeat.deadline(), None);
        assert!(!state.text_input_enabled);
        assert!(!state.ime.preedit_active());
        assert_eq!(
            state.take_events().input,
            vec![
                WaylandInputEvent::Modifiers(Modifiers::empty()),
                WaylandInputEvent::Focus(false),
            ]
        );
    }

    #[test]
    fn pointer_dispatch_never_synthesizes_keyboard_focus() {
        let source = include_str!("runtime.rs");
        let pointer_dispatch = source
            .split("impl Dispatch<wl_pointer::WlPointer, ()> for WaylandState")
            .nth(1)
            .and_then(|tail| {
                tail.split("impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandState")
                    .next()
            })
            .expect("pointer dispatch must remain present");
        assert!(!pointer_dispatch.contains("WaylandInputEvent::Focus"));

        let keyboard_dispatch = source
            .split("impl Dispatch<wl_keyboard::WlKeyboard, ()> for WaylandState")
            .nth(1)
            .and_then(|tail| {
                tail.split("impl Dispatch<zwp_text_input_v3::ZwpTextInputV3, ()> for WaylandState")
                    .next()
            })
            .expect("keyboard dispatch must remain present");
        assert!(keyboard_dispatch.contains("set_keyboard_focus(true)"));
        assert!(keyboard_dispatch.contains("set_keyboard_focus(false)"));
    }

    #[test]
    fn cursor_apply_prefers_shape_protocol_and_maps_all_cursor_kinds() {
        for (kind, expected) in [
            (CursorKind::Default, CursorShape::Default),
            (CursorKind::Pointer, CursorShape::Pointer),
            (CursorKind::Text, CursorShape::Text),
        ] {
            let plan = cursor_apply_plan(Some(41), kind, 1.5, true, None)
                .expect("enter serial should produce a cursor plan");
            assert_eq!(plan.serial, 41);
            assert_eq!(plan.kind, kind);
            assert_eq!(plan.path, CursorApplyPath::Shape(expected));
        }
    }

    #[test]
    fn cursor_apply_uses_shm_fallback_when_shape_protocol_is_absent() {
        let plan = cursor_apply_plan(Some(17), CursorKind::Text, 1.25, false, Some(2))
            .expect("fallback should remain available without cursor-shape");
        assert_eq!(plan.path, CursorApplyPath::Fallback { scale_bucket: 2 });
        assert_ne!(plan.path, CursorApplyPath::Disabled);
    }

    #[test]
    fn cursor_scale_bucket_is_stable_across_fractional_scale_events() {
        assert_eq!(cursor_scale_bucket(1.0), 1);
        assert_eq!(cursor_scale_bucket(1.25), 2);
        assert_eq!(cursor_scale_bucket(1.5), 2);
        assert_eq!(cursor_scale_bucket(2.0), 2);
    }

    #[test]
    fn cursor_fallback_preparation_skips_shape_path_and_prepares_absent_shape_once() {
        assert_eq!(
            cursor_fallback_preparation(true, true, CursorFallbackStatus::Uninitialized, 1.25,),
            CursorFallbackPreparation::ShapePath
        );
        assert_eq!(
            cursor_fallback_preparation(false, true, CursorFallbackStatus::Uninitialized, 1.25,),
            CursorFallbackPreparation::Prepare { scale_bucket: 2 }
        );
    }

    #[test]
    fn cursor_fallback_reuses_prepared_bucket_across_fractional_scale_events() {
        for scale in [1.25, 1.5, 2.0] {
            assert_eq!(
                cursor_fallback_preparation(
                    false,
                    true,
                    CursorFallbackStatus::Ready { scale_bucket: 2 },
                    scale,
                ),
                CursorFallbackPreparation::Ready
            );
        }
    }

    #[test]
    fn cursor_fallback_bucket_change_requests_lifecycle_rebuild() {
        assert_eq!(
            cursor_fallback_preparation(
                false,
                true,
                CursorFallbackStatus::Ready { scale_bucket: 1 },
                1.25,
            ),
            CursorFallbackPreparation::Prepare { scale_bucket: 2 }
        );
    }

    #[test]
    fn cursor_fallback_disabled_state_never_requests_preparation() {
        for scale in [1.0, 1.25, 2.0] {
            assert_eq!(
                cursor_fallback_preparation(false, true, CursorFallbackStatus::Disabled, scale,),
                CursorFallbackPreparation::Disabled
            );
        }
    }

    #[test]
    fn cursor_kinds_select_preloaded_fallback_slots() {
        assert_eq!(
            cursor_fallback_image_slot(CursorKind::Default),
            CursorFallbackImageSlot::Default
        );
        assert_eq!(
            cursor_fallback_image_slot(CursorKind::Pointer),
            CursorFallbackImageSlot::Pointer
        );
        assert_eq!(
            cursor_fallback_image_slot(CursorKind::Text),
            CursorFallbackImageSlot::Text
        );
    }

    #[test]
    fn cursor_kind_change_reuses_latest_enter_serial_with_preloaded_cache() {
        let default = cursor_apply_plan(Some(99), CursorKind::Default, 1.5, false, Some(2))
            .expect("fallback should use current enter serial");
        let pointer = cursor_apply_plan(Some(99), CursorKind::Pointer, 1.5, false, Some(2))
            .expect("cursor kind change should use current enter serial");
        assert_eq!(default.serial, 99);
        assert_eq!(pointer.serial, 99);
        assert_eq!(pointer.kind, CursorKind::Pointer);
        assert_eq!(pointer.path, CursorApplyPath::Fallback { scale_bucket: 2 });
    }

    #[test]
    fn cursor_theme_names_include_standard_xcursor_aliases() {
        assert_eq!(cursor_names_for_kind(CursorKind::Default)[0], "default");
        assert!(cursor_names_for_kind(CursorKind::Default).contains(&"left_ptr"));
        assert_eq!(cursor_names_for_kind(CursorKind::Pointer)[0], "pointer");
        assert!(cursor_names_for_kind(CursorKind::Pointer).contains(&"hand2"));
        assert_eq!(cursor_names_for_kind(CursorKind::Text)[0], "text");
        assert!(cursor_names_for_kind(CursorKind::Text).contains(&"xterm"));
    }

    #[test]
    fn logical_to_physical_uses_fractional_scale_without_reverse_rounding() {
        assert_eq!(logical_to_physical(800, 600, 1.25), (1000, 750));
        assert_eq!(logical_to_physical(800, 600, 1.5), (1200, 900));
        assert_eq!(logical_to_physical(801, 601, 1.25), (1001, 751));
    }

    #[test]
    fn repeated_scale_changes_recompute_from_logical_size_without_drift() {
        let logical = (801, 601);
        let _ = logical_to_physical(logical.0, logical.1, 1.25);
        let _ = logical_to_physical(logical.0, logical.1, 1.5);
        assert_eq!(logical_to_physical(logical.0, logical.1, 1.0), logical);
    }

    #[test]
    fn event_loop_timeout_rounds_up_and_sleeps_without_deadline() {
        let now = Instant::now();
        assert_eq!(poll_timeout_millis(None, now), -1);
        assert_eq!(poll_timeout_millis(Some(now), now), 0);
        assert_eq!(
            poll_timeout_millis(Some(now + Duration::from_micros(1500)), now),
            2
        );
    }

    #[test]
    fn frame_schedule_has_one_outstanding_callback_and_sleeps_when_idle() {
        let frame = FrameSchedule::default();
        assert!(!frame.ready_requested());
        assert!(!frame.should_arm_callback());

        frame.request();
        assert!(frame.take_ready());
        frame.mark_presented();
        assert!(!frame.callback_outstanding());
        assert!(!frame.should_arm_callback());

        frame.request();
        assert!(frame.should_arm_callback());
        frame.mark_callback_outstanding();
        frame.request();
        assert!(!frame.should_arm_callback());
        assert!(frame.callback_outstanding());

        frame.callback_done();
        assert!(frame.take_ready());
        assert!(!frame.callback_outstanding());
        assert!(!frame.ready_requested());
    }

    #[test]
    fn frame_schedule_keeps_animation_dirty_until_callback() {
        let frame = FrameSchedule::default();
        frame.request();
        assert!(frame.take_ready());
        frame.mark_presented();

        frame.request();
        frame.mark_callback_outstanding();
        frame.request();
        assert!(!frame.take_ready());
        frame.callback_done();
        assert!(frame.take_ready());
    }

    #[test]
    fn render_is_suppressed_until_configured_and_for_zero_physical_size() {
        assert!(!surface_renderable(false, 100, 100));
        assert!(!surface_renderable(true, 0, 100));
        assert!(!surface_renderable(true, 100, 0));
        assert!(surface_renderable(true, 100, 100));
    }
}
