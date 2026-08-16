use crate::renderer::Renderer;
use glutin::config::{Config, ConfigTemplateBuilder, GlConfig};
use glutin::context::{
    ContextApi, ContextAttributesBuilder, GlContext, GlProfile, NotCurrentContext,
    NotCurrentGlContext, PossiblyCurrentContext, Priority, Version,
};
use glutin::display::{GetGlDisplay, GlDisplay};
use glutin::surface::{GlSurface, Surface, SwapInterval, WindowSurface};
use glutin_winit::DisplayBuilder;
use std::cmp::Reverse;
use std::num::NonZeroU32;
use std::sync::Arc;
use winit::dpi::LogicalSize;
use winit::event_loop::ActiveEventLoop;
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Window, WindowAttributes};

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

fn window_attributes(logical_width: f64, logical_height: f64) -> WindowAttributes {
    Window::default_attributes()
        .with_title("Ronsole")
        .with_name("ronsole", "ronsole")
        .with_inner_size(LogicalSize::new(logical_width, logical_height))
        .with_transparent(false)
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
    window: &Window,
    raw_window_handle: RawWindowHandle,
    not_current_context: NotCurrentContext,
) -> Result<(Surface<WindowSurface>, PossiblyCurrentContext, bool), String> {
    let size = window.inner_size();
    let attributes = glutin::surface::SurfaceAttributesBuilder::<WindowSurface>::new().build(
        raw_window_handle,
        NonZeroU32::new(size.width.max(1)).unwrap_or(NonZeroU32::MIN),
        NonZeroU32::new(size.height.max(1)).unwrap_or(NonZeroU32::MIN),
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
    pub settings_font_value: &'a str,
    pub settings_scroll_value: &'a str,
}

pub struct WindowRuntime {
    renderer: Renderer,
    config: Config,
    context: PossiblyCurrentContext,
    surface: Surface<WindowSurface>,
    window: Arc<Window>,
    requested_plan: GlContextPlan,
    requested_priority: GlContextPriorityRequest,
    swap_interval_applied: bool,
}

impl WindowRuntime {
    pub fn bootstrap(
        event_loop: &ActiveEventLoop,
        logical_width: f64,
        logical_height: f64,
        terminal_font_size: f32,
    ) -> Result<Self, String> {
        let template = ConfigTemplateBuilder::new()
            .with_transparency(false)
            .with_depth_size(0)
            .with_stencil_size(0);
        let display_builder = DisplayBuilder::new()
            .with_window_attributes(Some(window_attributes(logical_width, logical_height)));
        let (window, gl_config) = display_builder
            .build(event_loop, template, |configs| {
                configs
                    .max_by_key(|config| {
                        framebuffer_config_rank(config.hardware_accelerated(), config.num_samples())
                    })
                    .unwrap_or_else(|| panic!("no OpenGL framebuffer configuration is available"))
            })
            .map_err(|error| format!("Wayland window/display creation failed: {error}"))?;
        let window = window.ok_or_else(|| "Wayland backend did not create a window".to_string())?;
        window.set_ime_allowed(true);

        let raw_window_handle = window
            .window_handle()
            .map_err(|error| format!("window handle is unavailable: {error}"))?
            .as_raw();
        let (not_current_context, requested_plan, requested_priority) =
            create_not_current_context(&gl_config, raw_window_handle)?;
        let (surface, context, swap_interval_applied) = create_surface_and_context(
            &gl_config,
            &window,
            raw_window_handle,
            not_current_context,
        )?;
        let renderer = Renderer::new(
            create_glow_context(&gl_config),
            window.scale_factor() as f32,
            terminal_font_size,
        )
        .map_err(|error| format!("renderer initialization failed: {error}"))?;
        let mut runtime = Self {
            renderer,
            config: gl_config,
            context,
            surface,
            window: Arc::new(window),
            requested_plan,
            requested_priority,
            swap_interval_applied,
        };
        let size = runtime.window.inner_size();
        if size.width > 0 && size.height > 0 {
            runtime.renderer.resize(size.width, size.height);
        }
        trim_allocator_after_gl_bootstrap();
        Ok(runtime)
    }

    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn window_arc(&self) -> Arc<Window> {
        Arc::clone(&self.window)
    }

    pub fn update_scale_factor(&mut self, scale_factor: f32) {
        self.renderer.update_scale_factor(scale_factor);
    }

    pub(crate) fn set_terminal_font_size(&mut self, logical_size: f32) -> bool {
        self.renderer.set_terminal_font_size(logical_size)
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
            params.settings_font_value,
            params.settings_scroll_value,
        );
        self.surface
            .swap_buffers(&self.context)
            .map_err(|error| format!("swap buffers failed: {error}"))?;
        Ok(layout)
    }

    pub fn terminal_tab_hit_test(&self, x: f32, y: f32) -> crate::renderer::TerminalTabHit {
        self.renderer.terminal_tab_hit_test(x, y)
    }

    pub(crate) fn settings_hit_test(
        &self,
        progress: f32,
        x: f32,
        y: f32,
    ) -> crate::renderer::SettingsHit {
        self.renderer.settings_hit_test(progress, x, y)
    }

    pub fn terminal_tab_strip_layout(&self) -> crate::renderer::TerminalTabStripLayout {
        self.renderer.terminal_tab_strip_layout()
    }

    pub fn terminal_tab_drag_destination(
        &self,
        drag: &crate::tabs::TabDragState,
    ) -> Option<usize> {
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

    pub fn terminal_search_cursor_from_x(
        &mut self,
        text: &str,
        x: f32,
        scroll_x: f32,
    ) -> usize {
        self.renderer.terminal_cursor_from_input_x(text, x, scroll_x)
    }

    pub fn diagnostics_report(&self) -> String {
        let size = self.window.inner_size();
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
            size.width,
            size.height,
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
        assert_eq!(blocking_swap_interval(), SwapInterval::Wait(NonZeroU32::MIN));
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
}
