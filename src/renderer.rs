mod terminal_ui;
pub(crate) use terminal_ui::{
    SettingsHit, TerminalTabHit, TerminalTabStripLayout, TerminalUiLayout,
    terminal_scrollbar_drag_target,
};
use glow::HasContext;
use rustc_hash::FxHashMap;
use std::sync::Arc;
use swash::FontRef;
use swash::scale::{
    Render, ScaleContext, Source, StrikeWith,
    image::{Content, Image},
};

const MAX_VERTICES: usize = 32_768;
const ATLAS_SIZE_W: i32 = 1024;
const ATLAS_SIZE_H: i32 = 1024;
const PRIMARY_ATLAS_INTERNAL_FORMAT: u32 = glow::R8;
const PRIMARY_ATLAS_UPLOAD_FORMAT: u32 = glow::RED;
const COLOR_ATLAS_MODE: f32 = 10.0;
const SOLID_RECT_MODE: f32 = 2.0;
const GLYPH_PRESENTATION_AUTO: u8 = 0;
const GLYPH_PRESENTATION_TEXT: u8 = 1;
const GLYPH_PRESENTATION_EMOJI: u8 = 2;
const DEFAULT_ACCENT_COLOR: [f32; 4] = [114.0 / 255.0, 89.0 / 255.0, 175.0 / 255.0, 1.0];
const UI_FONT_LOGICAL_SIZE: f32 = 18.0;
const LEGACY_TERMINAL_FONT_LOGICAL_SIZE: f32 = 18.0;
const LEGACY_TERMINAL_LINE_HEIGHT: f32 = 26.0;
const LEGACY_TERMINAL_BASELINE_OFFSET: f32 = 19.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct TerminalFontMetrics {
    raster_size: f32,
    line_height: f32,
    baseline_offset: f32,
}

fn terminal_font_metrics(logical_size: f32, scale_factor: f32) -> TerminalFontMetrics {
    let logical_size = if logical_size.is_finite() && logical_size > 0.0 {
        logical_size
    } else {
        crate::config::DEFAULT_TERMINAL_FONT_SIZE
    };
    let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let ratio = logical_size / LEGACY_TERMINAL_FONT_LOGICAL_SIZE;
    TerminalFontMetrics {
        raster_size: logical_size * scale_factor,
        line_height: (LEGACY_TERMINAL_LINE_HEIGHT * ratio * scale_factor)
            .round()
            .max(1.0),
        baseline_offset: (LEGACY_TERMINAL_BASELINE_OFFSET * ratio * scale_factor)
            .round()
            .max(1.0),
    }
}

fn ui_font_size(scale_factor: f32) -> f32 {
    let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    UI_FONT_LOGICAL_SIZE * scale_factor
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct TerminalPalette {
    bg: [f32; 4],
    fg: [f32; 4],
    minimap_bg: [f32; 4],
    accent: [f32; 4],
}

impl TerminalPalette {
    fn new(accent: [f32; 4]) -> Self {
        Self {
            bg: [0.156, 0.164, 0.211, 1.0],
            fg: [0.972, 0.972, 0.949, 1.0],
            minimap_bg: [0.129, 0.133, 0.172, 1.0],
            accent,
        }
    }

    fn accent_with_alpha(self, alpha: f32) -> [f32; 4] {
        [self.accent[0], self.accent[1], self.accent[2], alpha]
    }
}

fn parse_kde_color(content: &str, target_group: &str, target_key: &str) -> Option<[f32; 4]> {
    let mut current_group = None;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if let Some(group) = line.strip_prefix('[').and_then(|line| line.strip_suffix(']')) {
            current_group = Some(group.trim());
            continue;
        }
        if current_group != Some(target_group) {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != target_key {
            continue;
        }
        let mut parts = value.split(',');
        let parse_component = |part: &str| {
            part.trim()
                .parse::<f32>()
                .ok()
                .filter(|value| value.is_finite() && (0.0..=255.0).contains(value))
        };
        let r = parse_component(parts.next()?);
        let g = parse_component(parts.next()?);
        let b = parse_component(parts.next()?);
        if parts.next().is_some() {
            return None;
        }
        let (Some(r), Some(g), Some(b)) = (r, g, b) else {
            return None;
        };
        return Some([r / 255.0, g / 255.0, b / 255.0, 1.0]);
    }
    None
}

fn terminal_accent_from_kde_content(content: Option<&str>) -> [f32; 4] {
    content
        .and_then(|content| parse_kde_color(content, "Colors:Selection", "BackgroundNormal"))
        .unwrap_or(DEFAULT_ACCENT_COLOR)
}

fn load_terminal_palette() -> TerminalPalette {
    let kde_content = crate::platform::config_home_dir()
        .and_then(|root| std::fs::read_to_string(root.join("kdeglobals")).ok());
    TerminalPalette::new(terminal_accent_from_kde_content(kde_content.as_deref()))
}

const TERMINAL_FONT: &[u8] = include_bytes!("fonts/JetBrainsMonoNerdFont-Regular.ttf");
const UI_FONT: &[u8] = include_bytes!("fonts/Inter-Regular.otf");

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
struct Vertex {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [f32; 4],
    mode: f32,
    sdf_params: [f32; 3],
}

unsafe impl bytemuck::Zeroable for Vertex {}
unsafe impl bytemuck::Pod for Vertex {}

#[derive(Clone, Copy, Debug)]
struct GlyphInfo {
    u: f32,
    v: f32,
    uw: f32,
    vh: f32,
    width: f32,
    height: f32,
    offset_x: f32,
    offset_y: f32,
    advance: f32,
    mode: f32,
}

#[derive(Clone, Copy)]
struct AtlasEntry {
    u: f32,
    v: f32,
    uw: f32,
    vh: f32,
}

#[derive(Clone)]
enum FontSource {
    Static(&'static [u8]),
    Mapped(Arc<memmap2::Mmap>),
}

#[derive(Clone)]
struct FontData {
    source: FontSource,
    index: u32,
}

impl FontData {
    fn new_static(data: &'static [u8]) -> Self {
        Self {
            source: FontSource::Static(data),
            index: 0,
        }
    }

    fn map_first(paths: &[&str]) -> Option<Self> {
        for path in paths {
            let Ok(file) = std::fs::File::open(path) else {
                continue;
            };
            let Ok(mapping) = (unsafe { memmap2::Mmap::map(&file) }) else {
                continue;
            };
            return Some(Self {
                source: FontSource::Mapped(Arc::new(mapping)),
                index: 0,
            });
        }
        None
    }

    fn map_all_into(paths: &[&str], output: &mut Vec<Self>) {
        for path in paths {
            let Ok(file) = std::fs::File::open(path) else {
                continue;
            };
            let Ok(mapping) = (unsafe { memmap2::Mmap::map(&file) }) else {
                continue;
            };
            output.push(Self {
                source: FontSource::Mapped(Arc::new(mapping)),
                index: 0,
            });
        }
    }

    fn data(&self) -> &[u8] {
        match &self.source {
            FontSource::Static(data) => data,
            FontSource::Mapped(mapping) => mapping,
        }
    }
}

struct RasterizedGlyph {
    image: Option<Image>,
    advance: f32,
}

#[derive(Clone, Debug)]
pub struct GraphicsDiagnostics {
    pub vendor: String,
    pub renderer: String,
    pub version: String,
    pub shading_language: String,
}

pub struct Renderer {
    gl: glow::Context,
    diagnostics: GraphicsDiagnostics,
    program: glow::Program,
    proj_loc: Option<glow::UniformLocation>,
    vao: glow::VertexArray,
    vbo: glow::Buffer,
    alpha_texture: glow::Texture,
    color_texture: Option<glow::Texture>,
    vertices: Vec<Vertex>,
    fonts: Vec<FontData>,
    ui_fonts: Vec<FontData>,
    scale_context: ScaleContext,
    glyphs: FxHashMap<(char, u8), GlyphInfo>,
    ui_glyphs: FxHashMap<char, GlyphInfo>,
    alpha_atlas_x: i32,
    alpha_atlas_y: i32,
    alpha_row_h: i32,
    color_atlas_x: i32,
    color_atlas_y: i32,
    color_row_h: i32,
    ascii_advances: [f32; 128],
    terminal_font_logical_size: f32,
    terminal_font_size: f32,
    ui_font_size: f32,
    scale_factor: f32,
    width: f32,
    height: f32,
    line_height: f32,
    baseline_offset: f32,
    scratch_buffer: String,
    search_icons: [Option<GlyphInfo>; 4],
    terminal_tab_display_titles: Vec<String>,
    terminal_tab_widths: Vec<f32>,
    terminal_tab_actual_xs: Vec<f32>,
    terminal_tab_order: Vec<usize>,
    terminal_tab_render_order: Vec<usize>,
    terminal_tab_x_anim: Vec<f32>,
    terminal_tab_hitboxes: Vec<terminal_ui::TerminalTabHitbox>,
    terminal_tab_strip_layout: TerminalTabStripLayout,
    terminal_tab_base_x: f32,
    terminal_tab_animation_active: bool,
    palette: TerminalPalette,
}

impl Renderer {
    pub fn new(
        gl: glow::Context,
        scale_factor: f32,
        terminal_font_logical_size: f32,
    ) -> Result<Self, String> {
        let scale_factor = scale_factor.max(0.1);
        let terminal_font_logical_size =
            crate::config::normalize_terminal_font_size(terminal_font_logical_size);
        let terminal_metrics = terminal_font_metrics(terminal_font_logical_size, scale_factor);
        let diagnostics = unsafe {
            GraphicsDiagnostics {
                vendor: gl.get_parameter_string(glow::VENDOR),
                renderer: gl.get_parameter_string(glow::RENDERER),
                version: gl.get_parameter_string(glow::VERSION),
                shading_language: gl.get_parameter_string(glow::SHADING_LANGUAGE_VERSION),
            }
        };
        let is_gles = diagnostics.version.contains("OpenGL ES");
        if !graphics_version_supported(&diagnostics.version, is_gles) {
            let requirement = if is_gles {
                "OpenGL ES 3.0"
            } else {
                "OpenGL 3.3"
            };
            return Err(format!(
                "{requirement} or newer is required; detected {} ({})",
                diagnostics.version, diagnostics.renderer
            ));
        }

        let shader_preamble = if is_gles {
            "#version 300 es\nprecision highp float;\nprecision highp int;\n"
        } else {
            "#version 330 core\n"
        };
        let (program, proj_loc) = create_program(&gl, shader_preamble)?;
        let (vao, vbo) = create_vertex_buffer(&gl, program)?;
        let alpha_texture = create_alpha_texture(&gl)?;

        let mut fonts = Vec::with_capacity(12);
        if let Some(font) = FontData::map_first(&[
            "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
            "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        ]) {
            fonts.push(font);
        }
        fonts.push(FontData::new_static(TERMINAL_FONT));
        FontData::map_all_into(
            &[
                "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf",
                "/usr/share/fonts/noto/NotoSansSymbols-Regular.ttf",
                "/usr/share/fonts/TTF/NotoSansSymbols2-Regular.ttf",
                "/usr/share/fonts/TTF/NotoSansSymbols-Regular.ttf",
                "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
                "/usr/share/fonts/truetype/noto/NotoSansSymbols-Regular.ttf",
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/TTF/DejaVuSans.ttf",
                "/usr/share/fonts/truetype/ancient-scripts/Symbola_hint.ttf",
            ],
            &mut fonts,
        );
        if let Some(font) = FontData::map_first(&[
            "/usr/share/fonts/noto/NotoColorEmoji.ttf",
            "/usr/share/fonts/noto-emoji/NotoColorEmoji.ttf",
            "/usr/share/fonts/noto/NotoColorEmoji.google.ttf",
        ]) {
            fonts.push(font);
        }

        let mut ui_fonts = Vec::with_capacity(fonts.len() + 3);
        ui_fonts.push(FontData::new_static(UI_FONT));
        if let Some(font) = FontData::map_first(&[
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/TTF/NotoSans-Regular.ttf",
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/Inter/Inter-Regular.ttf",
        ]) {
            ui_fonts.push(font);
        }
        ui_fonts.push(FontData::new_static(TERMINAL_FONT));
        ui_fonts.extend(fonts.iter().cloned());

        let palette = load_terminal_palette();
        let mut renderer = Self {
            gl,
            diagnostics,
            program,
            proj_loc,
            vao,
            vbo,
            alpha_texture,
            color_texture: None,
            vertices: Vec::with_capacity(MAX_VERTICES),
            fonts,
            ui_fonts,
            scale_context: ScaleContext::new(),
            glyphs: FxHashMap::default(),
            ui_glyphs: FxHashMap::default(),
            alpha_atlas_x: 2,
            alpha_atlas_y: 2,
            alpha_row_h: 0,
            color_atlas_x: 2,
            color_atlas_y: 2,
            color_row_h: 0,
            ascii_advances: [0.0; 128],
            terminal_font_logical_size,
            terminal_font_size: terminal_metrics.raster_size,
            ui_font_size: ui_font_size(scale_factor),
            scale_factor,
            width: 1.0,
            height: 1.0,
            line_height: terminal_metrics.line_height,
            baseline_offset: terminal_metrics.baseline_offset,
            scratch_buffer: String::with_capacity(128),
            search_icons: [None; 4],
            terminal_tab_display_titles: Vec::with_capacity(8),
            terminal_tab_widths: Vec::with_capacity(8),
            terminal_tab_actual_xs: Vec::with_capacity(8),
            terminal_tab_order: Vec::with_capacity(8),
            terminal_tab_render_order: Vec::with_capacity(8),
            terminal_tab_x_anim: Vec::with_capacity(8),
            terminal_tab_hitboxes: Vec::with_capacity(8),
            terminal_tab_strip_layout: TerminalTabStripLayout::default(),
            terminal_tab_base_x: 0.0,
            terminal_tab_animation_active: false,
            palette,
        };
        renderer.prewarm_ascii();
        renderer.prewarm_search_icons();
        Ok(renderer)
    }

    pub fn graphics_diagnostics(&self) -> &GraphicsDiagnostics {
        &self.diagnostics
    }

    pub fn scale_factor(&self) -> f32 {
        self.scale_factor
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.width = width as f32;
        self.height = height as f32;
        unsafe {
            self.gl.viewport(0, 0, width as i32, height as i32);
        }
    }

    pub fn update_scale_factor(&mut self, scale_factor: f32) {
        if !scale_factor.is_finite()
            || scale_factor <= 0.0
            || (self.scale_factor - scale_factor).abs() < 0.001
        {
            return;
        }
        self.flush();
        self.scale_factor = scale_factor;
        let terminal_metrics = terminal_font_metrics(self.terminal_font_logical_size, scale_factor);
        self.terminal_font_size = terminal_metrics.raster_size;
        self.ui_font_size = ui_font_size(scale_factor);
        self.line_height = terminal_metrics.line_height;
        self.baseline_offset = terminal_metrics.baseline_offset;
        self.reset_atlases();
        self.prewarm_ascii();
        self.prewarm_search_icons();
        self.terminal_tab_x_anim.clear();
        self.terminal_tab_hitboxes.clear();
        self.terminal_tab_strip_layout = TerminalTabStripLayout::default();
    }

    pub(crate) fn set_terminal_font_size(&mut self, logical_size: f32) -> bool {
        let logical_size = crate::config::normalize_terminal_font_size(logical_size);
        if (self.terminal_font_logical_size - logical_size).abs() < 0.001 {
            return false;
        }
        self.flush();
        self.terminal_font_logical_size = logical_size;
        let terminal_metrics = terminal_font_metrics(logical_size, self.scale_factor);
        self.terminal_font_size = terminal_metrics.raster_size;
        self.line_height = terminal_metrics.line_height;
        self.baseline_offset = terminal_metrics.baseline_offset;
        self.reset_atlases();
        self.prewarm_ascii();
        self.prewarm_search_icons();
        true
    }

    fn prewarm_ascii(&mut self) {
        self.ascii_advances.fill(0.0);
        for byte in 32..128u8 {
            if let Some(glyph) = self.get_glyph(byte as char, None) {
                self.ascii_advances[byte as usize] = glyph.advance;
            }
        }
    }

    #[inline]
    fn snapped_text_advance(advance: f32, scale: f32) -> f32 {
        let px = (advance * scale).round();
        if px <= 0.0 && advance > 0.0 { 1.0 } else { px }
    }

    #[inline]
    fn fallback_ui_text_advance(scale: f32) -> f32 {
        (10.0 * scale).round().max(1.0)
    }

    #[inline]
    fn one_line_ui_char_is_zero_width(c: char) -> bool {
        matches!(c, '\n' | '\r' | '\u{FE0E}' | '\u{FE0F}' | '\u{200D}')
    }

    #[inline]
    fn one_line_ui_char_advance(c: char, glyph_advance: Option<f32>, scale: f32) -> f32 {
        if Self::one_line_ui_char_is_zero_width(c) {
            return 0.0;
        }
        glyph_advance
            .map(|advance| Self::snapped_text_advance(advance, scale))
            .unwrap_or_else(|| Self::fallback_ui_text_advance(scale))
    }

    #[inline]
    fn one_line_ui_char_layout(&mut self, c: char, scale: f32) -> (Option<GlyphInfo>, f32) {
        if Self::one_line_ui_char_is_zero_width(c) {
            return (None, Self::one_line_ui_char_advance(c, None, scale));
        }
        let glyph = self.get_ui_glyph(c);
        let advance =
            Self::one_line_ui_char_advance(c, glyph.map(|glyph| glyph.advance), scale);
        (glyph, advance)
    }

    fn draw_ui_text(
        &mut self,
        text: &str,
        mut x: f32,
        baseline_y: f32,
        color: [f32; 4],
        scale: f32,
    ) {
        for c in text.chars() {
            let (glyph, advance) = self.one_line_ui_char_layout(c, scale);
            if let Some(glyph) = glyph {
                self.push_glyph(x, baseline_y, glyph, color, scale);
            }
            x = (x + advance).round();
        }
    }

    fn get_ui_glyph(&mut self, c: char) -> Option<GlyphInfo> {
        if let Some(glyph) = self.ui_glyphs.get(&c) {
            return Some(*glyph);
        }
        let prefer_color = default_emoji_presentation(c);
        let rasterized = rasterize_glyph(
            &self.ui_fonts,
            &mut self.scale_context,
            c,
            self.ui_font_size,
            prefer_color,
        );
        let glyph = self.finish_rasterized_glyph(c, rasterized, prefer_color, true)?;
        self.ui_glyphs.insert(c, glyph);
        Some(glyph)
    }

    fn get_glyph(&mut self, c: char, prefer_color: Option<bool>) -> Option<GlyphInfo> {
        let presentation = match prefer_color {
            Some(false) => GLYPH_PRESENTATION_TEXT,
            Some(true) => GLYPH_PRESENTATION_EMOJI,
            None => GLYPH_PRESENTATION_AUTO,
        };
        let cache_key = (c, presentation);
        if let Some(glyph) = self.glyphs.get(&cache_key) {
            return Some(*glyph);
        }
        let prefer_color = prefer_color.unwrap_or_else(|| default_emoji_presentation(c));
        let rasterized = rasterize_glyph(
            &self.fonts,
            &mut self.scale_context,
            c,
            self.terminal_font_size,
            prefer_color,
        );
        let glyph = if let Some(glyph) =
            self.finish_rasterized_glyph(c, rasterized, prefer_color, false)
        {
            glyph
        } else if c != '□' {
            self.get_glyph('□', Some(false))?
        } else {
            return None;
        };
        self.glyphs.insert(cache_key, glyph);
        Some(glyph)
    }

    fn finish_rasterized_glyph(
        &mut self,
        c: char,
        rasterized: Option<RasterizedGlyph>,
        prefer_color: bool,
        ui: bool,
    ) -> Option<GlyphInfo> {
        let rasterized = rasterized?;
        let Some(image) = rasterized.image else {
            return Some(GlyphInfo {
                u: 0.0,
                v: 0.0,
                uw: 0.0,
                vh: 0.0,
                width: 0.0,
                height: 0.0,
                offset_x: 0.0,
                offset_y: 0.0,
                advance: rasterized.advance,
                mode: 0.0,
            });
        };

        let width = image.placement.width as i32;
        let height = image.placement.height as i32;
        if c.is_whitespace() || width <= 0 || height <= 0 {
            return Some(GlyphInfo {
                u: 0.0,
                v: 0.0,
                uw: 0.0,
                vh: 0.0,
                width: 0.0,
                height: 0.0,
                offset_x: 0.0,
                offset_y: 0.0,
                advance: rasterized.advance,
                mode: 0.0,
            });
        }

        let color = image.content == Content::Color && prefer_color;
        let entry = if color {
            self.upload_color_rgba(width, height, &image.data)?
        } else {
            self.upload_alpha(width, height, &image.data)?
        };
        Some(GlyphInfo {
            u: entry.u,
            v: entry.v,
            uw: entry.uw,
            vh: entry.vh,
            width: width as f32,
            height: height as f32,
            offset_x: image.placement.left as f32,
            offset_y: image.placement.top as f32
                - if ui {
                    self.scale_factor.round().max(1.0)
                } else {
                    0.0
                },
            advance: rasterized.advance,
            mode: if color { COLOR_ATLAS_MODE } else { 0.0 },
        })
    }

    fn push_glyph(
        &mut self,
        x: f32,
        baseline_y: f32,
        glyph: GlyphInfo,
        color: [f32; 4],
        scale: f32,
    ) {
        if glyph.width <= 0.0 || glyph.height <= 0.0 {
            return;
        }
        let (x, y, width, height) = glyph_quad_rect(x, baseline_y, glyph, scale);
        self.push_quad(
            x,
            y,
            width,
            height,
            glyph.u,
            glyph.v,
            glyph.uw,
            glyph.vh,
            color,
            glyph.mode,
            [0.0, 0.0, 0.0],
        );
    }

    fn push_rect(&mut self, x: f32, y: f32, width: f32, height: f32, color: [f32; 4]) {
        self.push_quad(
            x,
            y,
            width,
            height,
            -1.0,
            -1.0,
            0.0,
            0.0,
            color,
            SOLID_RECT_MODE,
            [0.0, 0.0, 0.0],
        );
    }

    fn push_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        radius: f32,
        color: [f32; 4],
    ) {
        let width = width.round();
        let height = height.round();
        let x1 = x.round();
        let y1 = y.round();
        let x2 = (x + width).round();
        let y2 = (y + height).round();
        let half_w = width * 0.5;
        let half_h = height * 0.5;
        self.ensure_vertex_capacity(6);
        self.vertices.extend_from_slice(&[
            Vertex {
                pos: [x1, y1],
                uv: [-half_w, -half_h],
                color,
                mode: 3.0,
                sdf_params: [half_w, half_h, radius],
            },
            Vertex {
                pos: [x2, y1],
                uv: [half_w, -half_h],
                color,
                mode: 3.0,
                sdf_params: [half_w, half_h, radius],
            },
            Vertex {
                pos: [x2, y2],
                uv: [half_w, half_h],
                color,
                mode: 3.0,
                sdf_params: [half_w, half_h, radius],
            },
            Vertex {
                pos: [x1, y1],
                uv: [-half_w, -half_h],
                color,
                mode: 3.0,
                sdf_params: [half_w, half_h, radius],
            },
            Vertex {
                pos: [x2, y2],
                uv: [half_w, half_h],
                color,
                mode: 3.0,
                sdf_params: [half_w, half_h, radius],
            },
            Vertex {
                pos: [x1, y2],
                uv: [-half_w, half_h],
                color,
                mode: 3.0,
                sdf_params: [half_w, half_h, radius],
            },
        ]);
    }

    fn push_quad(
        &mut self,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        u: f32,
        v: f32,
        uw: f32,
        vh: f32,
        color: [f32; 4],
        mode: f32,
        sdf_params: [f32; 3],
    ) {
        self.ensure_vertex_capacity(6);
        self.vertices.extend_from_slice(&quad_vertices(
            x, y, width, height, u, v, uw, vh, color, mode, sdf_params,
        ));
    }

    fn ensure_vertex_capacity(&mut self, additional: usize) {
        if self.vertices.len().saturating_add(additional) > MAX_VERTICES {
            self.flush();
        }
    }

    fn set_clip(&self, x: f32, y: f32, width: f32, height: f32) {
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let left = x.round().clamp(0.0, self.width) as i32;
        let top = y.round().clamp(0.0, self.height) as i32;
        let right = (x + width).round().clamp(0.0, self.width) as i32;
        let bottom = (y + height).round().clamp(0.0, self.height) as i32;
        let clip_w = (right - left).max(0);
        let clip_h = (bottom - top).max(0);
        unsafe {
            self.gl.enable(glow::SCISSOR_TEST);
            self.gl
                .scissor(left, self.height as i32 - bottom, clip_w, clip_h);
        }
    }

    fn clear_clip(&self) {
        unsafe {
            self.gl.disable(glow::SCISSOR_TEST);
        }
    }

    fn flush(&mut self) {
        if self.vertices.is_empty() {
            return;
        }
        let vertex_count = self.vertices.len().min(MAX_VERTICES);
        let projection = [
            2.0 / self.width.max(1.0),
            0.0,
            0.0,
            0.0,
            0.0,
            -2.0 / self.height.max(1.0),
            0.0,
            0.0,
            0.0,
            0.0,
            -1.0,
            0.0,
            -1.0,
            1.0,
            0.0,
            1.0,
        ];
        unsafe {
            self.gl.use_program(Some(self.program));
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl
                .uniform_matrix_4_f32_slice(self.proj_loc.as_ref(), false, &projection);
            self.gl.active_texture(glow::TEXTURE0);
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.alpha_texture));
            if let Some(color_texture) = self.color_texture {
                self.gl.active_texture(glow::TEXTURE1);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(color_texture));
                self.gl.active_texture(glow::TEXTURE0);
            }
            self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            self.gl.buffer_sub_data_u8_slice(
                glow::ARRAY_BUFFER,
                0,
                bytemuck::cast_slice(&self.vertices[..vertex_count]),
            );
            self.gl.draw_arrays(glow::TRIANGLES, 0, vertex_count as i32);
        }
        self.vertices.clear();
    }

    fn upload_alpha(&mut self, width: i32, height: i32, data: &[u8]) -> Option<AtlasEntry> {
        if width <= 0 || height <= 0 || data.len() != width as usize * height as usize {
            return None;
        }
        if self.alpha_atlas_x + width + 2 > ATLAS_SIZE_W {
            self.alpha_atlas_x = 2;
            self.alpha_atlas_y += self.alpha_row_h + 2;
            self.alpha_row_h = 0;
        }
        if self.alpha_atlas_y + height + 2 > ATLAS_SIZE_H {
            self.reset_atlases();
        }
        if self.alpha_atlas_x + width + 2 > ATLAS_SIZE_W
            || self.alpha_atlas_y + height + 2 > ATLAS_SIZE_H
        {
            return None;
        }
        let x = self.alpha_atlas_x;
        let y = self.alpha_atlas_y;
        unsafe {
            self.gl.active_texture(glow::TEXTURE0);
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.alpha_texture));
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            self.gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x,
                y,
                width,
                height,
                PRIMARY_ATLAS_UPLOAD_FORMAT,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(data)),
            );
        }
        self.alpha_atlas_x += width + 2;
        self.alpha_row_h = self.alpha_row_h.max(height);
        Some(AtlasEntry {
            u: x as f32 / ATLAS_SIZE_W as f32,
            v: y as f32 / ATLAS_SIZE_H as f32,
            uw: width as f32 / ATLAS_SIZE_W as f32,
            vh: height as f32 / ATLAS_SIZE_H as f32,
        })
    }

    fn upload_color_rgba(
        &mut self,
        width: i32,
        height: i32,
        data: &[u8],
    ) -> Option<AtlasEntry> {
        if width <= 0 || height <= 0 || data.len() != width as usize * height as usize * 4 {
            return None;
        }
        let color_texture = self.ensure_color_texture()?;
        if self.color_atlas_x + width + 2 > ATLAS_SIZE_W {
            self.color_atlas_x = 2;
            self.color_atlas_y += self.color_row_h + 2;
            self.color_row_h = 0;
        }
        if self.color_atlas_y + height + 2 > ATLAS_SIZE_H {
            self.reset_color_atlas();
        }
        if self.color_atlas_x + width + 2 > ATLAS_SIZE_W
            || self.color_atlas_y + height + 2 > ATLAS_SIZE_H
        {
            return None;
        }
        let x = self.color_atlas_x;
        let y = self.color_atlas_y;
        unsafe {
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(color_texture));
            self.gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x,
                y,
                width,
                height,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(data)),
            );
            self.gl.active_texture(glow::TEXTURE0);
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.alpha_texture));
        }
        self.color_atlas_x += width + 2;
        self.color_row_h = self.color_row_h.max(height);
        Some(AtlasEntry {
            u: x as f32 / ATLAS_SIZE_W as f32,
            v: y as f32 / ATLAS_SIZE_H as f32,
            uw: width as f32 / ATLAS_SIZE_W as f32,
            vh: height as f32 / ATLAS_SIZE_H as f32,
        })
    }

    fn ensure_color_texture(&mut self) -> Option<glow::Texture> {
        if let Some(texture) = self.color_texture {
            return Some(texture);
        }
        let texture = unsafe {
            let texture = self.gl.create_texture().ok()?;
            self.gl.active_texture(glow::TEXTURE1);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                ATLAS_SIZE_W,
                ATLAS_SIZE_H,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            self.gl.active_texture(glow::TEXTURE0);
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.alpha_texture));
            texture
        };
        self.color_texture = Some(texture);
        Some(texture)
    }

    fn reset_atlases(&mut self) {
        self.glyphs.clear();
        self.ui_glyphs.clear();
        self.search_icons.fill(None);
        self.alpha_atlas_x = 2;
        self.alpha_atlas_y = 2;
        self.alpha_row_h = 0;
        self.color_atlas_x = 2;
        self.color_atlas_y = 2;
        self.color_row_h = 0;
        unsafe {
            self.gl.active_texture(glow::TEXTURE0);
            self.gl
                .bind_texture(glow::TEXTURE_2D, Some(self.alpha_texture));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                PRIMARY_ATLAS_INTERNAL_FORMAT as i32,
                ATLAS_SIZE_W,
                ATLAS_SIZE_H,
                0,
                PRIMARY_ATLAS_UPLOAD_FORMAT,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            if let Some(color_texture) = self.color_texture {
                self.gl.active_texture(glow::TEXTURE1);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(color_texture));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    ATLAS_SIZE_W,
                    ATLAS_SIZE_H,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                self.gl.active_texture(glow::TEXTURE0);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(self.alpha_texture));
            }
        }
    }

    fn reset_color_atlas(&mut self) {
        self.glyphs.clear();
        self.ui_glyphs.clear();
        self.color_atlas_x = 2;
        self.color_atlas_y = 2;
        self.color_row_h = 0;
        if let Some(color_texture) = self.color_texture {
            unsafe {
                self.gl.active_texture(glow::TEXTURE1);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(color_texture));
                self.gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    ATLAS_SIZE_W,
                    ATLAS_SIZE_H,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(None),
                );
                self.gl.active_texture(glow::TEXTURE0);
                self.gl
                    .bind_texture(glow::TEXTURE_2D, Some(self.alpha_texture));
            }
        }
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        unsafe {
            if let Some(color_texture) = self.color_texture.take() {
                self.gl.delete_texture(color_texture);
            }
            self.gl.delete_texture(self.alpha_texture);
            self.gl.delete_buffer(self.vbo);
            self.gl.delete_vertex_array(self.vao);
            self.gl.delete_program(self.program);
        }
    }
}

fn create_program(
    gl: &glow::Context,
    shader_preamble: &str,
) -> Result<(glow::Program, Option<glow::UniformLocation>), String> {
    unsafe {
        let vertex_shader = gl
            .create_shader(glow::VERTEX_SHADER)
            .map_err(|error| format!("failed to create vertex shader: {error}"))?;
        let vertex_source = format!(
            "{shader_preamble}in vec2 pos; in vec2 uv; in vec4 color; in float mode; in vec3 sdf_params;\nout vec2 v_uv; out vec4 v_color; out float v_mode; flat out vec3 v_sdf_params;\nuniform mat4 proj;\nvoid main() {{ gl_Position = proj * vec4(pos, 0.0, 1.0); v_uv = uv; v_color = color; v_mode = mode; v_sdf_params = sdf_params; }}"
        );
        gl.shader_source(vertex_shader, &vertex_source);
        gl.compile_shader(vertex_shader);
        if !gl.get_shader_compile_status(vertex_shader) {
            let log = gl.get_shader_info_log(vertex_shader);
            gl.delete_shader(vertex_shader);
            return Err(format!("vertex shader compilation failed: {log}"));
        }

        let fragment_shader = gl
            .create_shader(glow::FRAGMENT_SHADER)
            .map_err(|error| format!("failed to create fragment shader: {error}"))?;
        let fragment_source = format!(
            "{shader_preamble}in vec2 v_uv; in vec4 v_color; in float v_mode; flat in vec3 v_sdf_params;\nout vec4 out_color;\nuniform sampler2D tex; uniform sampler2D color_tex;\nfloat rounded_box_sdf(vec2 p, vec2 size, float radius) {{ return length(max(abs(p) - size + radius, 0.0)) - radius; }}\nvoid main() {{\n if (v_mode == 2.0) {{ out_color = v_color; }}\n else if (v_mode == 3.0) {{ float d = rounded_box_sdf(v_uv, vec2(v_sdf_params.x, v_sdf_params.y), v_sdf_params.z); float a = 1.0 - smoothstep(-0.5, 0.5, d); if (a <= 0.0) discard; out_color = vec4(v_color.rgb, v_color.a * a); }}\n else if (v_mode == 10.0) {{ out_color = texture(color_tex, v_uv) * v_color; }}\n else {{ float a = texture(tex, v_uv).r; out_color = vec4(v_color.rgb, a * v_color.a); }}\n}}"
        );
        gl.shader_source(fragment_shader, &fragment_source);
        gl.compile_shader(fragment_shader);
        if !gl.get_shader_compile_status(fragment_shader) {
            let log = gl.get_shader_info_log(fragment_shader);
            gl.delete_shader(vertex_shader);
            gl.delete_shader(fragment_shader);
            return Err(format!("fragment shader compilation failed: {log}"));
        }

        let program = gl
            .create_program()
            .map_err(|error| format!("failed to create shader program: {error}"))?;
        gl.attach_shader(program, vertex_shader);
        gl.attach_shader(program, fragment_shader);
        gl.link_program(program);
        gl.delete_shader(vertex_shader);
        gl.delete_shader(fragment_shader);
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(format!("shader program link failed: {log}"));
        }

        gl.use_program(Some(program));
        gl.uniform_1_i32(gl.get_uniform_location(program, "tex").as_ref(), 0);
        gl.uniform_1_i32(gl.get_uniform_location(program, "color_tex").as_ref(), 1);
        Ok((program, gl.get_uniform_location(program, "proj")))
    }
}

fn create_vertex_buffer(
    gl: &glow::Context,
    program: glow::Program,
) -> Result<(glow::VertexArray, glow::Buffer), String> {
    unsafe {
        let vao = gl
            .create_vertex_array()
            .map_err(|error| format!("failed to create vertex array: {error}"))?;
        let vbo = gl
            .create_buffer()
            .map_err(|error| format!("failed to create vertex buffer: {error}"))?;
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_size(
            glow::ARRAY_BUFFER,
            (MAX_VERTICES * std::mem::size_of::<Vertex>()) as i32,
            glow::DYNAMIC_DRAW,
        );

        let stride = std::mem::size_of::<Vertex>() as i32;
        configure_attribute(gl, program, "pos", 2, stride, 0)?;
        configure_attribute(gl, program, "uv", 2, stride, 8)?;
        configure_attribute(gl, program, "color", 4, stride, 16)?;
        configure_attribute(gl, program, "mode", 1, stride, 32)?;
        configure_attribute(gl, program, "sdf_params", 3, stride, 36)?;

        gl.enable(glow::BLEND);
        gl.blend_func_separate(
            glow::SRC_ALPHA,
            glow::ONE_MINUS_SRC_ALPHA,
            glow::ZERO,
            glow::ONE,
        );
        Ok((vao, vbo))
    }
}

fn configure_attribute(
    gl: &glow::Context,
    program: glow::Program,
    name: &str,
    size: i32,
    stride: i32,
    offset: i32,
) -> Result<(), String> {
    let location = unsafe { gl.get_attrib_location(program, name) }
        .ok_or_else(|| format!("required shader attribute `{name}` is unavailable"))?;
    unsafe {
        gl.vertex_attrib_pointer_f32(location, size, glow::FLOAT, false, stride, offset);
        gl.enable_vertex_attrib_array(location);
    }
    Ok(())
}

fn create_alpha_texture(gl: &glow::Context) -> Result<glow::Texture, String> {
    unsafe {
        let texture = gl
            .create_texture()
            .map_err(|error| format!("failed to create glyph atlas texture: {error}"))?;
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(texture));
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_S,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_WRAP_T,
            glow::CLAMP_TO_EDGE as i32,
        );
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            PRIMARY_ATLAS_INTERNAL_FORMAT as i32,
            ATLAS_SIZE_W,
            ATLAS_SIZE_H,
            0,
            PRIMARY_ATLAS_UPLOAD_FORMAT,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        Ok(texture)
    }
}

fn rasterize_glyph(
    fonts: &[FontData],
    scale_context: &mut ScaleContext,
    c: char,
    font_size: f32,
    prefer_color: bool,
) -> Option<RasterizedGlyph> {
    if prefer_color {
        for font in fonts.iter().rev() {
            if let Some(glyph) = rasterize_from_font(font, scale_context, c, font_size, true) {
                return Some(glyph);
            }
        }
    } else {
        for font in fonts {
            if let Some(glyph) = rasterize_from_font(font, scale_context, c, font_size, false) {
                return Some(glyph);
            }
        }
    }
    None
}

fn rasterize_from_font(
    font_data: &FontData,
    scale_context: &mut ScaleContext,
    c: char,
    font_size: f32,
    prefer_color: bool,
) -> Option<RasterizedGlyph> {
    let font_ref = FontRef::from_index(font_data.data(), font_data.index as usize)?;
    let glyph_id = font_ref.charmap().map(c);
    if glyph_id == 0 && !c.is_whitespace() {
        return None;
    }
    let metrics = font_ref.metrics(&[]);
    let advance = (font_ref.glyph_metrics(&[]).advance_width(glyph_id) as f32 * font_size)
        / metrics.units_per_em as f32;
    let mut scaler = scale_context
        .builder(font_ref)
        .size(font_size)
        .hint(true)
        .build();
    let image = Render::new(&[
        Source::ColorOutline(0),
        Source::ColorBitmap(StrikeWith::BestFit),
        Source::Outline,
    ])
    .render(&mut scaler, glyph_id)
    .filter(|image| image.content != Content::Color || prefer_color);
    if image.is_none() && !c.is_whitespace() {
        return None;
    }
    Some(RasterizedGlyph { image, advance })
}

#[inline(always)]
fn default_emoji_presentation(c: char) -> bool {
    matches!(
        c as u32,
        0x231A..=0x231B
            | 0x23E9..=0x23F3
            | 0x25FB..=0x25FE
            | 0x2614..=0x2615
            | 0x2648..=0x2653
            | 0x267F
            | 0x2693
            | 0x26A1
            | 0x26AA..=0x26AB
            | 0x26BD..=0x26BE
            | 0x26C4..=0x26C5
            | 0x26CE
            | 0x26D4
            | 0x26EA
            | 0x26F2..=0x26F3
            | 0x26F5
            | 0x26FA
            | 0x26FD
            | 0x2705
            | 0x270A..=0x270B
            | 0x2728
            | 0x274C
            | 0x274E
            | 0x2753..=0x2755
            | 0x2757
            | 0x2795..=0x2797
            | 0x27B0
            | 0x27BF
            | 0x2B07
            | 0x2B1B..=0x2B1C
            | 0x2B50
            | 0x2B55
            | 0x1F000..=0x1FAFF
            | 0x1FC00..=0x1FFFF
    )
}

#[inline(always)]
pub(crate) fn terminal_force_text_presentation(c: char) -> bool {
    matches!(c, '✔' | '✓')
}

fn parse_graphics_version(version: &str) -> Option<(u8, u8)> {
    let numeric = version
        .split_whitespace()
        .find(|part| part.as_bytes().first().is_some_and(u8::is_ascii_digit))?;
    let mut parts = numeric.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .ok()?;
    Some((major, minor))
}

fn graphics_version_supported(version: &str, is_gles: bool) -> bool {
    parse_graphics_version(version).is_some_and(|version| {
        if is_gles {
            version >= (3, 0)
        } else {
            version >= (3, 3)
        }
    })
}

fn glyph_quad_rect(
    x: f32,
    baseline_y: f32,
    glyph: GlyphInfo,
    scale: f32,
) -> (f32, f32, f32, f32) {
    (
        x + glyph.offset_x * scale,
        baseline_y - glyph.offset_y * scale,
        glyph.width * scale,
        glyph.height * scale,
    )
}

#[allow(clippy::too_many_arguments)]
fn quad_vertices(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    u: f32,
    v: f32,
    uw: f32,
    vh: f32,
    color: [f32; 4],
    mode: f32,
    sdf_params: [f32; 3],
) -> [Vertex; 6] {
    let x1 = x.round();
    let y1 = y.round();
    let x2 = (x + width).round();
    let y2 = (y + height).round();
    let v1 = Vertex {
        pos: [x1, y1],
        uv: [u, v],
        color,
        mode,
        sdf_params,
    };
    let v2 = Vertex {
        pos: [x2, y1],
        uv: [u + uw, v],
        color,
        mode,
        sdf_params,
    };
    let v3 = Vertex {
        pos: [x2, y2],
        uv: [u + uw, v + vh],
        color,
        mode,
        sdf_params,
    };
    let v4 = Vertex {
        pos: [x1, y2],
        uv: [u, v + vh],
        color,
        mode,
        sdf_params,
    };
    [v1, v2, v3, v1, v3, v4]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_geometry_is_pixel_snapped() {
        let vertices = quad_vertices(
            1.2,
            2.6,
            10.1,
            20.2,
            0.1,
            0.2,
            0.3,
            0.4,
            [1.0; 4],
            0.0,
            [0.0; 3],
        );
        assert_eq!(vertices[0].pos, [1.0, 3.0]);
        assert_eq!(vertices[1].pos, [11.0, 3.0]);
        assert_eq!(vertices[2].pos, [11.0, 23.0]);
        assert_eq!(vertices[5].pos, [1.0, 23.0]);
    }

    #[test]
    fn solid_rect_quad_mode_is_opaque_not_rounded_sdf() {
        let vertices = quad_vertices(
            1.2,
            2.6,
            10.1,
            20.2,
            -1.0,
            -1.0,
            0.0,
            0.0,
            [1.0; 4],
            SOLID_RECT_MODE,
            [0.0; 3],
        );
        assert!(vertices.iter().all(|vertex| vertex.mode == SOLID_RECT_MODE));
        assert!(vertices.iter().all(|vertex| vertex.sdf_params == [0.0; 3]));
        assert_ne!(SOLID_RECT_MODE, 3.0);
    }

    #[test]
    fn glyph_quad_keeps_shared_baseline_rounding() {
        let glyph = GlyphInfo {
            u: 0.0,
            v: 0.0,
            uw: 0.0,
            vh: 0.0,
            width: 7.25,
            height: 10.49,
            offset_x: 0.0,
            offset_y: 12.51,
            advance: 8.0,
            mode: 0.0,
        };
        let (_, y, _, height) = glyph_quad_rect(10.0, 100.0, glyph, 1.0);
        let vertices = quad_vertices(
            10.0,
            y,
            glyph.width,
            height,
            0.0,
            0.0,
            0.0,
            0.0,
            [1.0; 4],
            0.0,
            [0.0; 3],
        );
        assert_eq!(vertices[0].pos[1], 87.0);
        assert_eq!(vertices[2].pos[1], 98.0);
    }

    #[test]
    fn graphics_version_parser_accepts_desktop_and_gles() {
        assert_eq!(parse_graphics_version("4.1.0 NVIDIA"), Some((4, 1)));
        assert_eq!(parse_graphics_version("OpenGL ES 3.2 Mesa"), Some((3, 2)));
        assert!(graphics_version_supported("3.3 Mesa", false));
        assert!(graphics_version_supported("OpenGL ES 3.0", true));
        assert!(!graphics_version_supported("3.2 Mesa", false));
    }

    #[test]
    fn renderer_batch_and_atlas_bounds_match_rriter_baseline() {
        assert_eq!(ATLAS_SIZE_W, 1024);
        assert_eq!(ATLAS_SIZE_H, 1024);
        assert_eq!(PRIMARY_ATLAS_INTERNAL_FORMAT, glow::R8);
        assert_eq!(PRIMARY_ATLAS_UPLOAD_FORMAT, glow::RED);
        assert!(MAX_VERTICES >= 32_768);
    }

    #[test]
    fn terminal_font_metrics_preserve_legacy_proportions_and_scale() {
        assert_eq!(
            terminal_font_metrics(18.0, 1.0),
            TerminalFontMetrics {
                raster_size: 18.0,
                line_height: 26.0,
                baseline_offset: 19.0,
            }
        );
        assert_eq!(
            terminal_font_metrics(18.0, 1.5),
            TerminalFontMetrics {
                raster_size: 27.0,
                line_height: 39.0,
                baseline_offset: 29.0,
            }
        );
        let smaller = terminal_font_metrics(16.0, 1.0);
        assert!(smaller.raster_size < 18.0);
        assert!(smaller.line_height < 26.0);
        assert!(smaller.baseline_offset < 19.0);
        for scale in [1.25, 1.3333334, 1.5] {
            let metrics = terminal_font_metrics(16.0, scale);
            assert!(metrics.raster_size.is_finite() && metrics.raster_size > 0.0);
            assert!(metrics.line_height.is_finite() && metrics.line_height >= 1.0);
            assert!(metrics.baseline_offset.is_finite() && metrics.baseline_offset >= 1.0);
            assert_eq!(metrics.line_height.fract(), 0.0);
            assert_eq!(metrics.baseline_offset.fract(), 0.0);
        }
    }

    #[test]
    fn ui_font_raster_size_is_independent_of_terminal_font_setting() {
        let scale = 1.5;
        let ui = ui_font_size(scale);
        assert_eq!(ui, 27.0);
        assert_ne!(terminal_font_metrics(16.0, scale).raster_size, ui);
        assert_eq!(ui_font_size(scale), ui);
    }

    #[test]
    fn emoji_default_selection_keeps_text_symbols_text_first() {
        assert!(!default_emoji_presentation('✔'));
        assert!(!default_emoji_presentation('✓'));
        assert!(default_emoji_presentation('✅'));
        assert!(default_emoji_presentation('😀'));
    }

    #[test]
    fn kde_selection_color_parser_accepts_exact_group_key_and_whitespace() {
        let content = "[Other]\nBackgroundNormal=1,2,3\n  [Colors:Selection]  \n BackgroundNormal = 114, 89, 175 \n";
        assert_eq!(
            parse_kde_color(content, "Colors:Selection", "BackgroundNormal"),
            Some(DEFAULT_ACCENT_COLOR)
        );
        assert_eq!(
            parse_kde_color(content, "Other", "BackgroundNormal"),
            Some([1.0 / 255.0, 2.0 / 255.0, 3.0 / 255.0, 1.0])
        );
    }

    #[test]
    fn kde_selection_color_parser_rejects_wrong_or_invalid_values() {
        assert!(parse_kde_color(
            "[Colors:Window]\nBackgroundNormal=114,89,175",
            "Colors:Selection",
            "BackgroundNormal"
        )
        .is_none());
        assert!(parse_kde_color(
            "[Colors:Selection]\nForegroundNormal=114,89,175",
            "Colors:Selection",
            "BackgroundNormal"
        )
        .is_none());
        for value in ["nope,89,175", "256,89,175", "114,-1,175", "114,89,175,4"] {
            let content = format!("[Colors:Selection]\nBackgroundNormal={value}");
            assert!(parse_kde_color(&content, "Colors:Selection", "BackgroundNormal").is_none());
        }
    }

    #[test]
    fn terminal_palette_uses_shared_accent_and_donor_fallback() {
        assert_eq!(terminal_accent_from_kde_content(None), DEFAULT_ACCENT_COLOR);
        assert_eq!(
            terminal_accent_from_kde_content(Some(
                "[Colors:Selection]\nBackgroundNormal=invalid"
            )),
            DEFAULT_ACCENT_COLOR
        );
        let accent = [0.1, 0.2, 0.3, 1.0];
        let palette = TerminalPalette::new(accent);
        assert_eq!(palette.accent, accent);
        assert_eq!(palette.accent_with_alpha(0.6), [0.1, 0.2, 0.3, 0.6]);
        assert_eq!(palette.bg, [0.156, 0.164, 0.211, 1.0]);
        assert_eq!(palette.fg, [0.972, 0.972, 0.949, 1.0]);
        assert_eq!(palette.minimap_bg, [0.129, 0.133, 0.172, 1.0]);
    }
}
