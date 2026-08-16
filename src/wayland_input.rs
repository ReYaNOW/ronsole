use crate::input_types::{
    CursorKind, KeyCode, KeyInput, KeyState, Modifiers, PhysicalKey, PointerButton,
    PointerPosition, ScrollDelta,
};
use memmap2::MmapOptions;
use std::collections::VecDeque;
use std::ffi::c_char;
use std::os::fd::OwnedFd;
use std::ptr::NonNull;
use std::time::{Duration, Instant};
use xkbcommon_dl::{
    XKB_MOD_NAME_ALT, XKB_MOD_NAME_CTRL, XKB_MOD_NAME_LOGO, XKB_MOD_NAME_SHIFT, XkbCommon,
    xkb_context, xkb_context_flags, xkb_keymap, xkb_keymap_compile_flags, xkb_keymap_format,
    xkb_state, xkb_state_component, xkbcommon_option,
};

pub(crate) const INPUT_QUEUE_CAPACITY: usize = 512;
const MAX_XKB_KEYMAP_BYTES: usize = 4 * 1024 * 1024;
const MAX_XKB_TEXT_BYTES: usize = 256;
const XKB_KEYCODE_OFFSET: u32 = 8;

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WaylandInputEvent {
    Focus(bool),
    Modifiers(Modifiers),
    Key(KeyInput),
    Text(String),
    ImeCommit(String),
    PointerMotion(PointerPosition),
    PointerLeave,
    PointerButton(KeyState, PointerButton),
    Scroll(ScrollDelta),
}

pub(crate) fn push_bounded_input_event(
    queue: &mut VecDeque<WaylandInputEvent>,
    event: WaylandInputEvent,
) {
    if matches!(event, WaylandInputEvent::PointerMotion(_))
        && let Some(WaylandInputEvent::PointerMotion(last)) = queue.back_mut()
        && let WaylandInputEvent::PointerMotion(next) = event
    {
        *last = next;
        return;
    }
    if matches!(event, WaylandInputEvent::Modifiers(_))
        && let Some(WaylandInputEvent::Modifiers(last)) = queue.back_mut()
        && let WaylandInputEvent::Modifiers(next) = event
    {
        *last = next;
        return;
    }
    if queue.len() >= INPUT_QUEUE_CAPACITY {
        if let Some(index) = queue.iter().position(|queued| {
            matches!(
                queued,
                WaylandInputEvent::PointerMotion(_) | WaylandInputEvent::Scroll(_)
            )
        }) {
            let _ = queue.remove(index);
        } else {
            let _ = queue.pop_front();
        }
    }
    queue.push_back(event);
}

pub(crate) fn physical_key_from_evdev(key: u32) -> PhysicalKey {
    let code = match key {
        1 => KeyCode::Escape,
        3 => KeyCode::Digit2,
        5 => KeyCode::Digit4,
        7 => KeyCode::Digit6,
        12 => KeyCode::Minus,
        14 => KeyCode::Backspace,
        15 => KeyCode::Tab,
        16 => KeyCode::KeyQ,
        17 => KeyCode::KeyW,
        18 => KeyCode::KeyE,
        19 => KeyCode::KeyR,
        20 => KeyCode::KeyT,
        21 => KeyCode::KeyY,
        22 => KeyCode::KeyU,
        23 => KeyCode::KeyI,
        24 => KeyCode::KeyO,
        25 => KeyCode::KeyP,
        26 => KeyCode::BracketLeft,
        27 => KeyCode::BracketRight,
        28 => KeyCode::Enter,
        30 => KeyCode::KeyA,
        31 => KeyCode::KeyS,
        32 => KeyCode::KeyD,
        33 => KeyCode::KeyF,
        34 => KeyCode::KeyG,
        35 => KeyCode::KeyH,
        36 => KeyCode::KeyJ,
        37 => KeyCode::KeyK,
        38 => KeyCode::KeyL,
        43 => KeyCode::Backslash,
        44 => KeyCode::KeyZ,
        45 => KeyCode::KeyX,
        46 => KeyCode::KeyC,
        47 => KeyCode::KeyV,
        48 => KeyCode::KeyB,
        49 => KeyCode::KeyN,
        50 => KeyCode::KeyM,
        53 => KeyCode::Slash,
        57 => KeyCode::Space,
        59 => KeyCode::F1,
        60 => KeyCode::F2,
        61 => KeyCode::F3,
        62 => KeyCode::F4,
        63 => KeyCode::F5,
        64 => KeyCode::F6,
        65 => KeyCode::F7,
        66 => KeyCode::F8,
        67 => KeyCode::F9,
        68 => KeyCode::F10,
        87 => KeyCode::F11,
        88 => KeyCode::F12,
        96 => KeyCode::NumpadEnter,
        102 => KeyCode::Home,
        103 => KeyCode::ArrowUp,
        104 => KeyCode::PageUp,
        105 => KeyCode::ArrowLeft,
        106 => KeyCode::ArrowRight,
        107 => KeyCode::End,
        108 => KeyCode::ArrowDown,
        109 => KeyCode::PageDown,
        110 => KeyCode::Insert,
        111 => KeyCode::Delete,
        _ => return PhysicalKey::Unidentified,
    };
    PhysicalKey::Code(code)
}

pub(crate) fn pointer_button_from_linux(button: u32) -> PointerButton {
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;
    const BTN_SIDE: u32 = 0x113;
    const BTN_EXTRA: u32 = 0x114;
    const BTN_FORWARD: u32 = 0x115;
    const BTN_BACK: u32 = 0x116;

    match button {
        BTN_LEFT => PointerButton::Left,
        BTN_RIGHT => PointerButton::Right,
        BTN_MIDDLE => PointerButton::Middle,
        BTN_BACK | BTN_SIDE => PointerButton::Back,
        BTN_FORWARD | BTN_EXTRA => PointerButton::Forward,
        button => PointerButton::Other(button as u16),
    }
}

pub(crate) fn pointer_position(logical_x: f64, logical_y: f64, scale: f32) -> PointerPosition {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    PointerPosition {
        x: (logical_x * f64::from(scale)) as f32,
        y: (logical_y * f64::from(scale)) as f32,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum PointerAxisSource {
    #[default]
    Unknown,
    Wheel,
    Finger,
    Continuous,
    WheelTilt,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct PointerAxisFrame {
    horizontal: AxisValue,
    vertical: AxisValue,
    source: PointerAxisSource,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct AxisValue {
    absolute: f64,
    discrete: i32,
    value120: i32,
    has_absolute: bool,
    has_discrete: bool,
    has_value120: bool,
}

impl PointerAxisFrame {
    pub(crate) fn set_source(&mut self, source: PointerAxisSource) {
        self.source = source;
    }

    pub(crate) fn set_absolute(&mut self, horizontal: bool, value: f64) {
        let axis = if horizontal {
            &mut self.horizontal
        } else {
            &mut self.vertical
        };
        axis.absolute += value;
        axis.has_absolute = true;
    }

    pub(crate) fn add_discrete(&mut self, horizontal: bool, value: i32) {
        let axis = if horizontal {
            &mut self.horizontal
        } else {
            &mut self.vertical
        };
        axis.discrete = axis.discrete.saturating_add(value);
        axis.has_discrete = true;
    }

    pub(crate) fn add_value120(&mut self, horizontal: bool, value: i32) {
        let axis = if horizontal {
            &mut self.horizontal
        } else {
            &mut self.vertical
        };
        axis.value120 = axis.value120.saturating_add(value);
        axis.has_value120 = true;
    }

    pub(crate) fn take_scroll(&mut self, scale: f32) -> Option<ScrollDelta> {
        let frame = std::mem::take(self);
        let continuous_source = matches!(
            frame.source,
            PointerAxisSource::Finger | PointerAxisSource::Continuous
        );
        if continuous_source && (frame.horizontal.has_absolute || frame.vertical.has_absolute) {
            return frame.pixel_delta(scale);
        }
        if frame.horizontal.has_value120 || frame.vertical.has_value120 {
            return Some(ScrollDelta::Line {
                x: -(frame.horizontal.value120 as f32) / 120.0,
                y: -(frame.vertical.value120 as f32) / 120.0,
            });
        }
        if frame.horizontal.has_discrete || frame.vertical.has_discrete {
            return Some(ScrollDelta::Line {
                x: -(frame.horizontal.discrete as f32),
                y: -(frame.vertical.discrete as f32),
            });
        }
        if frame.horizontal.has_absolute || frame.vertical.has_absolute {
            return frame.pixel_delta(scale);
        }
        None
    }

    fn pixel_delta(self, scale: f32) -> Option<ScrollDelta> {
        let scale = if scale.is_finite() && scale > 0.0 {
            scale
        } else {
            1.0
        };
        Some(ScrollDelta::Pixel {
            x: -(self.horizontal.absolute as f32) * scale,
            y: -(self.vertical.absolute as f32) * scale,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepeatConfig {
    rate: u32,
    delay: Duration,
}

impl Default for RepeatConfig {
    fn default() -> Self {
        Self {
            rate: 25,
            delay: Duration::from_millis(200),
        }
    }
}

impl RepeatConfig {
    pub(crate) fn from_wayland(rate: i32, delay_ms: i32) -> Option<Self> {
        let rate = u32::try_from(rate).ok().filter(|rate| *rate > 0)?;
        let delay_ms = u64::try_from(delay_ms).unwrap_or(0);
        Some(Self {
            rate,
            delay: Duration::from_millis(delay_ms),
        })
    }

    fn interval(self) -> Duration {
        Duration::from_nanos((1_000_000_000u64 / u64::from(self.rate)).max(1))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct KeyRepeatState {
    config: Option<RepeatConfig>,
    active_key: Option<u32>,
    deadline: Option<Instant>,
}

impl KeyRepeatState {
    pub(crate) fn with_default_config() -> Self {
        Self {
            config: Some(RepeatConfig::default()),
            active_key: None,
            deadline: None,
        }
    }

    pub(crate) fn set_config(&mut self, config: Option<RepeatConfig>) {
        self.config = config;
        if self.config.is_none() {
            self.stop();
        }
    }

    pub(crate) fn start(&mut self, key: u32, now: Instant) {
        let Some(config) = self.config else {
            self.stop();
            return;
        };
        self.active_key = Some(key);
        self.deadline = Some(now + config.delay);
    }

    pub(crate) fn stop_key(&mut self, key: u32) {
        if self.active_key == Some(key) {
            self.stop();
        }
    }

    pub(crate) fn stop(&mut self) {
        self.active_key = None;
        self.deadline = None;
    }

    pub(crate) fn deadline(self) -> Option<Instant> {
        self.deadline
    }

    pub(crate) fn take_due(&mut self, now: Instant) -> Option<u32> {
        let key = self.active_key?;
        let deadline = self.deadline?;
        if now < deadline {
            return None;
        }
        let config = self.config?;
        self.deadline = Some(now + config.interval());
        Some(key)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ImeBatch {
    pending_commit: Option<String>,
    pending_preedit: Option<String>,
    active_preedit: Option<String>,
}

impl ImeBatch {
    pub(crate) fn preedit(&mut self, text: Option<String>) {
        self.pending_preedit = text;
    }

    pub(crate) fn commit_string(&mut self, text: Option<String>) {
        self.pending_preedit = None;
        self.pending_commit = text.filter(|text| !text.is_empty());
    }

    pub(crate) fn done(&mut self) -> Option<String> {
        let commit = self.pending_commit.take();
        if commit.is_some() || self.pending_preedit.is_none() {
            self.active_preedit = None;
        }
        if let Some(preedit) = self.pending_preedit.take() {
            self.active_preedit = (!preedit.is_empty()).then_some(preedit);
        }
        commit
    }

    pub(crate) fn clear(&mut self) {
        self.pending_commit = None;
        self.pending_preedit = None;
        self.active_preedit = None;
    }

    pub(crate) fn preedit_active(&self) -> bool {
        self.active_preedit.is_some() || self.pending_preedit.is_some()
    }
}

pub(crate) fn should_emit_xkb_text(
    state: KeyState,
    physical_key: PhysicalKey,
    modifiers: Modifiers,
    text_input_enabled: bool,
    repeat: bool,
    ime_preedit_active: bool,
) -> bool {
    if state != KeyState::Pressed || modifiers.control_key() || modifiers.super_key() {
        return false;
    }
    if matches!(
        physical_key,
        PhysicalKey::Code(
            KeyCode::Enter
                | KeyCode::NumpadEnter
                | KeyCode::Backspace
                | KeyCode::Tab
                | KeyCode::Escape
        )
    ) {
        return false;
    }
    // text-input-v3 focus does not mean every hardware key becomes an IME
    // commit.  When an input method consumes a key the compositor owns that
    // key sequence; keys that still reach wl_keyboard must keep their XKB
    // text, matching the existing input contract. During an active preedit,
    // suppress synthetic repeat text so it cannot leak composition into PTY.
    if text_input_enabled && repeat && ime_preedit_active {
        return false;
    }
    true
}

pub(crate) fn cursor_shape_for_kind(kind: CursorKind) -> CursorShape {
    match kind {
        CursorKind::Default => CursorShape::Default,
        CursorKind::Pointer => CursorShape::Pointer,
        CursorKind::Text => CursorShape::Text,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorShape {
    Default,
    Pointer,
    Text,
}

pub(crate) struct XkbKeyboard {
    library: &'static XkbCommon,
    context: NonNull<xkb_context>,
    keymap: Option<NonNull<xkb_keymap>>,
    state: Option<NonNull<xkb_state>>,
    text_scratch: Vec<u8>,
}

impl XkbKeyboard {
    pub(crate) fn new() -> Result<Self, String> {
        let library = xkbcommon_option()
            .ok_or_else(|| "libxkbcommon is unavailable for Wayland keyboard input".to_string())?;
        let context = NonNull::new(unsafe {
            (library.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS)
        })
        .ok_or_else(|| "failed to create XKB context".to_string())?;
        Ok(Self {
            library,
            context,
            keymap: None,
            state: None,
            text_scratch: Vec::with_capacity(32),
        })
    }

    pub(crate) fn set_keymap(&mut self, fd: OwnedFd, size: u32) -> Result<(), String> {
        let size = usize::try_from(size).map_err(|_| "XKB keymap size is invalid".to_string())?;
        if size == 0 || size > MAX_XKB_KEYMAP_BYTES {
            return Err(format!(
                "XKB keymap size {size} is outside the accepted bound"
            ));
        }
        let map = unsafe {
            MmapOptions::new()
                .len(size)
                .map_copy_read_only(&fd)
                .map_err(|error| format!("failed to map XKB keymap fd: {error}"))?
        };
        let keymap = NonNull::new(unsafe {
            (self.library.xkb_keymap_new_from_buffer)(
                self.context.as_ptr(),
                map.as_ptr().cast(),
                size,
                xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .ok_or_else(|| "failed to compile compositor XKB keymap".to_string())?;
        self.install_keymap(keymap)
    }

    fn install_keymap(&mut self, keymap: NonNull<xkb_keymap>) -> Result<(), String> {
        let state = NonNull::new(unsafe { (self.library.xkb_state_new)(keymap.as_ptr()) });
        let Some(state) = state else {
            unsafe { (self.library.xkb_keymap_unref)(keymap.as_ptr()) };
            return Err("failed to create XKB state".to_string());
        };
        self.release_state_and_keymap();
        self.keymap = Some(keymap);
        self.state = Some(state);
        Ok(())
    }

    fn release_state_and_keymap(&mut self) {
        if let Some(state) = self.state.take() {
            unsafe { (self.library.xkb_state_unref)(state.as_ptr()) };
        }
        if let Some(keymap) = self.keymap.take() {
            unsafe { (self.library.xkb_keymap_unref)(keymap.as_ptr()) };
        }
    }

    pub(crate) fn update_modifiers(
        &mut self,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) -> Modifiers {
        let Some(state) = self.state else {
            return Modifiers::empty();
        };
        unsafe {
            (self.library.xkb_state_update_mask)(
                state.as_ptr(),
                depressed,
                latched,
                locked,
                0,
                0,
                group,
            );
        }
        self.modifiers()
    }

    pub(crate) fn clear_modifiers(&mut self) {
        let _ = self.update_modifiers(0, 0, 0, 0);
    }

    pub(crate) fn modifiers(&self) -> Modifiers {
        let Some(state) = self.state else {
            return Modifiers::empty();
        };
        let active = |name: &[u8]| unsafe {
            (self.library.xkb_state_mod_name_is_active)(
                state.as_ptr(),
                name.as_ptr().cast::<c_char>(),
                xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
            ) > 0
        };
        Modifiers::new(
            active(XKB_MOD_NAME_SHIFT),
            active(XKB_MOD_NAME_CTRL),
            active(XKB_MOD_NAME_ALT),
            active(XKB_MOD_NAME_LOGO),
        )
    }

    pub(crate) fn key_repeats(&self, evdev_key: u32) -> bool {
        let Some(keymap) = self.keymap else {
            return false;
        };
        let keycode = evdev_key.saturating_add(XKB_KEYCODE_OFFSET);
        unsafe { (self.library.xkb_keymap_key_repeats)(keymap.as_ptr(), keycode) != 0 }
    }

    pub(crate) fn text_for_key(&mut self, evdev_key: u32) -> Option<String> {
        let state = self.state?;
        let keycode = evdev_key.saturating_add(XKB_KEYCODE_OFFSET);
        let keysym = unsafe { (self.library.xkb_state_key_get_one_sym)(state.as_ptr(), keycode) };
        if keysym == 0 {
            return None;
        }
        let required = unsafe {
            (self.library.xkb_state_key_get_utf8)(state.as_ptr(), keycode, std::ptr::null_mut(), 0)
        };
        let required = usize::try_from(required).ok()?;
        if required == 0 || required > MAX_XKB_TEXT_BYTES {
            return None;
        }
        self.text_scratch.clear();
        self.text_scratch.reserve(required + 1);
        let written = unsafe {
            (self.library.xkb_state_key_get_utf8)(
                state.as_ptr(),
                keycode,
                self.text_scratch.as_mut_ptr().cast(),
                self.text_scratch.capacity(),
            )
        };
        if usize::try_from(written).ok()? != required {
            return None;
        }
        unsafe { self.text_scratch.set_len(required) };
        std::str::from_utf8(&self.text_scratch)
            .ok()
            .map(ToOwned::to_owned)
    }

    #[cfg(test)]
    fn set_layouts_for_test(&mut self, layouts: &str) -> Result<(), String> {
        use std::ffi::CString;
        use xkbcommon_dl::{xkb_keymap_compile_flags, xkb_rule_names};

        let layouts = CString::new(layouts).map_err(|_| "invalid test layout".to_string())?;
        let names = xkb_rule_names {
            rules: std::ptr::null(),
            model: std::ptr::null(),
            layout: layouts.as_ptr(),
            variant: std::ptr::null(),
            options: std::ptr::null(),
        };
        let keymap = NonNull::new(unsafe {
            (self.library.xkb_keymap_new_from_names)(
                self.context.as_ptr(),
                &names,
                xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            )
        })
        .ok_or_else(|| "failed to compile test XKB keymap".to_string())?;
        self.install_keymap(keymap)
    }

    #[cfg(test)]
    fn modifier_mask_for_test(&self, name: &[u8]) -> u32 {
        let keymap = self.keymap.expect("test keymap must be installed");
        let index = unsafe {
            (self.library.xkb_keymap_mod_get_index)(keymap.as_ptr(), name.as_ptr().cast())
        };
        assert_ne!(index, xkbcommon_dl::XKB_MOD_INVALID);
        assert!(index < u32::BITS);
        1u32 << index
    }
}

impl Drop for XkbKeyboard {
    fn drop(&mut self) {
        self.release_state_and_keymap();
        unsafe { (self.library.xkb_context_unref)(self.context.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_mapping_uses_linux_evdev_positions_not_text_layout() {
        assert_eq!(
            physical_key_from_evdev(30),
            PhysicalKey::Code(KeyCode::KeyA)
        );
        assert_eq!(physical_key_from_evdev(59), PhysicalKey::Code(KeyCode::F1));
        assert_eq!(
            physical_key_from_evdev(103),
            PhysicalKey::Code(KeyCode::ArrowUp)
        );
        assert_eq!(
            physical_key_from_evdev(110),
            PhysicalKey::Code(KeyCode::Insert)
        );
    }

    #[test]
    fn xkb_layout_switch_changes_utf8_without_changing_physical_key() {
        let mut xkb = XkbKeyboard::new().expect("XKB library must be available for tests");
        xkb.set_layouts_for_test("us,ru")
            .expect("test keymap must compile");

        assert_eq!(xkb.text_for_key(30).as_deref(), Some("a"));
        let _ = xkb.update_modifiers(0, 0, 0, 1);
        assert_eq!(xkb.text_for_key(30).as_deref(), Some("ф"));
        assert_eq!(
            physical_key_from_evdev(30),
            PhysicalKey::Code(KeyCode::KeyA)
        );
    }

    #[test]
    fn xkb_modifier_transitions_use_compositor_masks() {
        let mut xkb = XkbKeyboard::new().expect("XKB library must be available for tests");
        xkb.set_layouts_for_test("us")
            .expect("test keymap must compile");

        let shift = xkb.modifier_mask_for_test(XKB_MOD_NAME_SHIFT);
        let control = xkb.modifier_mask_for_test(XKB_MOD_NAME_CTRL);
        let alt = xkb.modifier_mask_for_test(XKB_MOD_NAME_ALT);
        let logo = xkb.modifier_mask_for_test(XKB_MOD_NAME_LOGO);

        assert_eq!(xkb.update_modifiers(shift, 0, 0, 0), Modifiers::SHIFT);
        assert_eq!(
            xkb.update_modifiers(control | alt, 0, 0, 0),
            Modifiers::CONTROL | Modifiers::ALT
        );
        assert_eq!(xkb.update_modifiers(logo, 0, 0, 0), Modifiers::SUPER);
        assert!(xkb.update_modifiers(0, 0, 0, 0).is_empty());
    }

    #[test]
    fn xkb_text_routing_avoids_control_and_ime_duplicates() {
        let plain = Modifiers::empty();
        assert!(should_emit_xkb_text(
            KeyState::Pressed,
            PhysicalKey::Code(KeyCode::KeyA),
            plain,
            false,
            false,
            false,
        ));
        assert!(!should_emit_xkb_text(
            KeyState::Pressed,
            PhysicalKey::Code(KeyCode::Enter),
            plain,
            false,
            false,
            false,
        ));
        assert!(should_emit_xkb_text(
            KeyState::Pressed,
            PhysicalKey::Code(KeyCode::KeyA),
            plain,
            true,
            false,
            false,
        ));
        assert!(should_emit_xkb_text(
            KeyState::Pressed,
            PhysicalKey::Code(KeyCode::KeyA),
            plain,
            true,
            true,
            false,
        ));
        assert!(!should_emit_xkb_text(
            KeyState::Pressed,
            PhysicalKey::Code(KeyCode::KeyA),
            Modifiers::CONTROL,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn key_repeat_starts_stops_and_uses_deadlines_without_bursting() {
        let base = Instant::now();
        let mut repeat = KeyRepeatState::with_default_config();
        repeat.set_config(RepeatConfig::from_wayland(20, 300));
        repeat.start(30, base);
        assert_eq!(repeat.deadline(), Some(base + Duration::from_millis(300)));
        assert_eq!(repeat.take_due(base + Duration::from_millis(299)), None);
        assert_eq!(repeat.take_due(base + Duration::from_millis(300)), Some(30));
        assert_eq!(repeat.deadline(), Some(base + Duration::from_millis(350)));
        repeat.stop_key(30);
        assert_eq!(repeat.deadline(), None);
    }

    #[test]
    fn ime_batch_commits_once_and_never_exposes_preedit_as_committed_text() {
        let mut ime = ImeBatch::default();
        ime.preedit(Some("пр".to_string()));
        assert!(ime.preedit_active());
        assert_eq!(ime.done(), None);
        assert!(ime.preedit_active());

        ime.commit_string(Some("привет".to_string()));
        assert_eq!(ime.done().as_deref(), Some("привет"));
        assert_eq!(ime.done(), None);
        assert!(!ime.preedit_active());
    }

    #[test]
    fn pointer_axis_preserves_value120_and_touchpad_precision() {
        let mut wheel = PointerAxisFrame::default();
        wheel.add_value120(false, 30);
        assert_eq!(
            wheel.take_scroll(1.0),
            Some(ScrollDelta::Line { x: -0.0, y: -0.25 })
        );

        let mut touchpad = PointerAxisFrame::default();
        touchpad.set_source(PointerAxisSource::Finger);
        touchpad.set_absolute(true, 1.25);
        touchpad.set_absolute(false, -2.5);
        assert_eq!(
            touchpad.take_scroll(1.5),
            Some(ScrollDelta::Pixel { x: -1.875, y: 3.75 })
        );
    }

    #[test]
    fn pointer_button_mapping_preserves_middle_and_selection_left_button() {
        assert_eq!(pointer_button_from_linux(0x110), PointerButton::Left);
        assert_eq!(pointer_button_from_linux(0x112), PointerButton::Middle);
        assert_eq!(pointer_button_from_linux(0x111), PointerButton::Right);
    }

    #[test]
    fn bounded_input_queue_coalesces_motion_without_losing_capacity_bound() {
        let mut queue = VecDeque::new();
        push_bounded_input_event(
            &mut queue,
            WaylandInputEvent::PointerMotion(PointerPosition { x: 1.0, y: 1.0 }),
        );
        push_bounded_input_event(
            &mut queue,
            WaylandInputEvent::PointerMotion(PointerPosition { x: 2.0, y: 3.0 }),
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.pop_front(),
            Some(WaylandInputEvent::PointerMotion(PointerPosition {
                x: 2.0,
                y: 3.0
            }))
        );
    }
}
