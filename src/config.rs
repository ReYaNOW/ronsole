use std::fs;
use std::io;
use std::path::PathBuf;

pub(crate) const DEFAULT_WINDOW_WIDTH: f64 = 1100.0;
pub(crate) const DEFAULT_WINDOW_HEIGHT: f64 = 720.0;
pub(crate) const DEFAULT_TERMINAL_FONT_SIZE: f32 = 16.0;
pub(crate) const MIN_TERMINAL_FONT_SIZE: f32 = 10.0;
pub(crate) const MAX_TERMINAL_FONT_SIZE: f32 = 32.0;
pub(crate) const TERMINAL_FONT_SIZE_STEP: f32 = 1.0;
pub(crate) const DEFAULT_SCROLL_SENSITIVITY: f32 = 1.0;
pub(crate) const MIN_SCROLL_SENSITIVITY: f32 = 0.25;
pub(crate) const MAX_SCROLL_SENSITIVITY: f32 = 3.0;
pub(crate) const SCROLL_SENSITIVITY_STEP: f32 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RgbColor {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
}

impl RgbColor {
    pub(crate) const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub(crate) fn parse_hex(value: &str) -> Option<Self> {
        let bytes = value.as_bytes();
        if bytes.len() != 7 || bytes[0] != b'#' {
            return None;
        }
        let component = |hi: u8, lo: u8| {
            let nibble = |byte: u8| match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            };
            Some(nibble(hi)? << 4 | nibble(lo)?)
        };
        Some(Self::new(
            component(bytes[1], bytes[2])?,
            component(bytes[3], bytes[4])?,
            component(bytes[5], bytes[6])?,
        ))
    }

    pub(crate) fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    pub(crate) fn to_rgba(self) -> [f32; 4] {
        [
            f32::from(self.r) / 255.0,
            f32::from(self.g) / 255.0,
            f32::from(self.b) / 255.0,
            1.0,
        ]
    }
}

pub(crate) const DEFAULT_TERMINAL_BACKGROUND: RgbColor = RgbColor::new(0x28, 0x2A, 0x36);

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AppConfig {
    pub(crate) window_width: f64,
    pub(crate) window_height: f64,
    pub(crate) terminal_font_size: f32,
    pub(crate) scroll_sensitivity: f32,
    pub(crate) terminal_background: RgbColor,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            window_width: DEFAULT_WINDOW_WIDTH,
            window_height: DEFAULT_WINDOW_HEIGHT,
            terminal_font_size: DEFAULT_TERMINAL_FONT_SIZE,
            scroll_sensitivity: DEFAULT_SCROLL_SENSITIVITY,
            terminal_background: DEFAULT_TERMINAL_BACKGROUND,
        }
    }
}

impl AppConfig {
    pub(crate) fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        match fs::read_to_string(&path) {
            Ok(content) => parse_config_content(&content),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::default(),
            Err(error) => {
                eprintln!("Ronsole: failed to load config {}: {error}", path.display());
                Self::default()
            }
        }
    }

    pub(crate) fn save(&self) -> io::Result<()> {
        let Some(path) = config_path() else {
            return Ok(());
        };
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        fs::create_dir_all(parent)?;
        let temp = parent.join(format!(".config.{}.tmp", std::process::id()));
        if let Err(error) = fs::write(&temp, format_config_content(self)) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temp, &path) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn set_window_size(&mut self, width: f64, height: f64) -> bool {
        if !valid_window_dimension(width) || !valid_window_dimension(height) {
            return false;
        }
        if self.window_width == width && self.window_height == height {
            return false;
        }
        self.window_width = width;
        self.window_height = height;
        true
    }

    pub(crate) fn adjust_terminal_font_size(&mut self, delta: f32) -> bool {
        let next = normalize_terminal_font_size(self.terminal_font_size + delta);
        if (next - self.terminal_font_size).abs() < f32::EPSILON {
            return false;
        }
        self.terminal_font_size = next;
        true
    }

    pub(crate) fn adjust_scroll_sensitivity(&mut self, delta: f32) -> bool {
        let stepped = ((self.scroll_sensitivity + delta) * 100.0).round() / 100.0;
        let next = normalize_scroll_sensitivity(stepped);
        if (next - self.scroll_sensitivity).abs() < f32::EPSILON {
            return false;
        }
        self.scroll_sensitivity = next;
        true
    }
}

#[inline]
pub(crate) fn normalize_terminal_font_size(value: f32) -> f32 {
    if !value.is_finite() {
        DEFAULT_TERMINAL_FONT_SIZE
    } else {
        value.clamp(MIN_TERMINAL_FONT_SIZE, MAX_TERMINAL_FONT_SIZE)
    }
}

#[inline]
pub(crate) fn normalize_scroll_sensitivity(value: f32) -> f32 {
    if !value.is_finite() {
        DEFAULT_SCROLL_SENSITIVITY
    } else {
        value.clamp(MIN_SCROLL_SENSITIVITY, MAX_SCROLL_SENSITIVITY)
    }
}

fn valid_window_dimension(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn config_path() -> Option<PathBuf> {
    crate::platform::config_home_dir().map(|root| root.join("ronsole").join("config"))
}

fn parse_config_content(content: &str) -> AppConfig {
    let mut config = AppConfig::default();
    for raw_line in content.lines() {
        let line = raw_line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "window_width" => {
                config.window_width = parse_window_dimension(value, DEFAULT_WINDOW_WIDTH);
            }
            "window_height" => {
                config.window_height = parse_window_dimension(value, DEFAULT_WINDOW_HEIGHT);
            }
            "terminal_font_size" => {
                config.terminal_font_size = value
                    .parse::<f32>()
                    .ok()
                    .map(normalize_terminal_font_size)
                    .unwrap_or(DEFAULT_TERMINAL_FONT_SIZE);
            }
            "scroll_sensitivity" => {
                config.scroll_sensitivity = value
                    .parse::<f32>()
                    .ok()
                    .map(normalize_scroll_sensitivity)
                    .unwrap_or(DEFAULT_SCROLL_SENSITIVITY);
            }
            "terminal_background" => {
                config.terminal_background =
                    RgbColor::parse_hex(value).unwrap_or(DEFAULT_TERMINAL_BACKGROUND);
            }
            _ => {}
        }
    }
    config
}

fn parse_window_dimension(value: &str, default: f64) -> f64 {
    value
        .parse::<f64>()
        .ok()
        .filter(|value| valid_window_dimension(*value))
        .unwrap_or(default)
}

fn format_config_content(config: &AppConfig) -> String {
    format!(
        "window_width={:.3}\nwindow_height={:.3}\nterminal_font_size={:.2}\nscroll_sensitivity={:.2}\nterminal_background={}\n",
        config.window_width,
        config.window_height,
        config.terminal_font_size,
        config.scroll_sensitivity,
        config.terminal_background.to_hex(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_match_terminal_display_contract() {
        let config = AppConfig::default();
        assert_eq!(config.window_width, 1100.0);
        assert_eq!(config.window_height, 720.0);
        assert_eq!(config.terminal_font_size, 16.0);
        assert_eq!(config.scroll_sensitivity, 1.0);
        assert_eq!(config.terminal_background, DEFAULT_TERMINAL_BACKGROUND);
    }

    #[test]
    fn config_round_trip_preserves_supported_values() {
        let config = AppConfig {
            window_width: 1280.0,
            window_height: 800.0,
            terminal_font_size: 21.0,
            scroll_sensitivity: 1.75,
            terminal_background: RgbColor::new(0x12, 0xAB, 0xEF),
        };
        assert_eq!(
            parse_config_content(&format_config_content(&config)),
            config
        );
    }

    #[test]
    fn config_parser_ignores_unknown_and_recovers_from_malformed_values() {
        let config = parse_config_content(
            "unknown=42\nwindow_width=nope\nwindow_height=0\nterminal_font_size=NaN\nscroll_sensitivity=inf\nterminal_background=#12345Z\n",
        );
        assert_eq!(config, AppConfig::default());
    }

    #[test]
    fn config_parser_clamps_supported_settings_and_rejects_nonfinite_values() {
        let low = parse_config_content("terminal_font_size=-5\nscroll_sensitivity=-1\n");
        assert_eq!(low.terminal_font_size, MIN_TERMINAL_FONT_SIZE);
        assert_eq!(low.scroll_sensitivity, MIN_SCROLL_SENSITIVITY);

        let high = parse_config_content("terminal_font_size=100\nscroll_sensitivity=99\n");
        assert_eq!(high.terminal_font_size, MAX_TERMINAL_FONT_SIZE);
        assert_eq!(high.scroll_sensitivity, MAX_SCROLL_SENSITIVITY);

        for invalid in ["NaN", "inf", "-inf"] {
            let parsed = parse_config_content(&format!(
                "terminal_font_size={invalid}\nscroll_sensitivity={invalid}\n"
            ));
            assert_eq!(parsed.terminal_font_size, DEFAULT_TERMINAL_FONT_SIZE);
            assert_eq!(parsed.scroll_sensitivity, DEFAULT_SCROLL_SENSITIVITY);
        }
    }

    #[test]
    fn scroll_sensitivity_step_is_tenth_stable_and_clamped() {
        assert_eq!(SCROLL_SENSITIVITY_STEP, 0.1);
        let mut config = AppConfig::default();
        assert!(config.adjust_scroll_sensitivity(SCROLL_SENSITIVITY_STEP));
        assert!((config.scroll_sensitivity - 1.1).abs() < 0.0001);
        assert!(config.adjust_scroll_sensitivity(-SCROLL_SENSITIVITY_STEP));
        assert!((config.scroll_sensitivity - 1.0).abs() < 0.0001);
        for _ in 0..7 {
            assert!(config.adjust_scroll_sensitivity(-SCROLL_SENSITIVITY_STEP));
        }
        assert!((config.scroll_sensitivity - 0.3).abs() < 0.0001);
        assert!(config.adjust_scroll_sensitivity(-SCROLL_SENSITIVITY_STEP));
        assert_eq!(config.scroll_sensitivity, MIN_SCROLL_SENSITIVITY);
        assert!(!config.adjust_scroll_sensitivity(-SCROLL_SENSITIVITY_STEP));

        config.scroll_sensitivity = 2.95;
        assert!(config.adjust_scroll_sensitivity(SCROLL_SENSITIVITY_STEP));
        assert_eq!(config.scroll_sensitivity, MAX_SCROLL_SENSITIVITY);
        assert!(!config.adjust_scroll_sensitivity(SCROLL_SENSITIVITY_STEP));
        assert!(format_config_content(&config).contains("scroll_sensitivity=3.00\n"));
    }

    #[test]
    fn terminal_background_hex_is_strict_and_canonical() {
        for (text, expected) in [
            ("#000000", RgbColor::new(0, 0, 0)),
            ("#FFFFFF", RgbColor::new(255, 255, 255)),
            ("#12aBcF", RgbColor::new(0x12, 0xAB, 0xCF)),
        ] {
            let color = RgbColor::parse_hex(text).expect("valid #RRGGBB should parse");
            assert_eq!(color, expected);
            assert_eq!(RgbColor::parse_hex(&color.to_hex()), Some(expected));
        }
        for invalid in ["000000", "#00000", "#0000000", "#GG0000", "#12 456"] {
            assert_eq!(RgbColor::parse_hex(invalid), None);
        }
    }

    #[test]
    fn old_config_without_terminal_background_uses_default() {
        let config = parse_config_content("terminal_font_size=18\nscroll_sensitivity=1.50\n");
        assert_eq!(config.terminal_font_size, 18.0);
        assert_eq!(config.scroll_sensitivity, 1.5);
        assert_eq!(config.terminal_background, DEFAULT_TERMINAL_BACKGROUND);
    }

    #[test]
    fn terminal_background_rgba_conversion_is_exact_and_bounded() {
        assert_eq!(RgbColor::new(0, 0, 0).to_rgba(), [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(RgbColor::new(255, 255, 255).to_rgba(), [1.0, 1.0, 1.0, 1.0]);
        let rgba = RgbColor::new(0x28, 0x2A, 0x36).to_rgba();
        assert_eq!(rgba[0], 40.0 / 255.0);
        assert_eq!(rgba[1], 42.0 / 255.0);
        assert_eq!(rgba[2], 54.0 / 255.0);
        assert!(
            rgba.into_iter()
                .all(|component| (0.0..=1.0).contains(&component))
        );
    }
}
