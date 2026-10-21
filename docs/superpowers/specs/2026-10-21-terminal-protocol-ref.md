# Terminal escape-sequence reference

Byte notation below uses `ESC` = `0x1b`, `CSI` = `ESC [`, `OSC` = `ESC ]`, `ST` = `ESC \\` (bytes `1b 5c`), and `BEL` = `0x07`; examples use 7-bit controls unless stated. Parameters are ASCII decimal bytes. Primary references are linked per section. Items explicitly marked **Inference** are deductions, not normative text in the cited reference.

## 1. Kitty keyboard protocol

**Source:** [Kitty keyboard protocol](https://sw.kovidgoyal.net/kitty/keyboard-protocol/).

### Mode control and flags

| Bytes | Meaning |
|---|---|
| `CSI > flags u` | Push current keyboard flags, then set flags to `flags`; omitted flags means `0`. |
| `CSI < n u` | Pop `n` entries; omitted `n` means `1`. Popping past an empty stack resets flags. |
| `CSI = flags ; mode u` | Set/modify flags. `mode` defaults to `1`: replace all flags. `2`: set supplied bits, preserve others. `3`: clear supplied bits, preserve others. |
| `CSI ? u` → `CSI ? flags u` | Query and reply with current progressive-enhancement bitmask. |

Flag bits: `1` disambiguate escape codes; `2` report press/repeat/release; `4` report alternate key values; `8` report every key as an escape sequence; `16` include associated text (defined with flag 8). Terminals keep independent push/pop stacks for main and alternate screen. The spec recommends bounding stack depth. (Kitty protocol, “Progressive enhancement.”)

Encoding: `CSI unicode-key-code[:shifted-key[:base-layout-key]] ; modifiers[:event-type] [; text-codepoints] u`. The first key code is required; alternate-key subfields and subsequent parameters are optional. Empty subfields preserve positions (e.g. `code::base`). Text codepoints, when multiple, are colon-separated. Modifiers are a bitmask plus **1**: Shift=1, Alt=2, Ctrl=4, Super=8, Hyper=16, Meta=32, CapsLock=64, NumLock=128. Thus no modifiers is `1`, Shift is `2`, Ctrl+Shift is `6`. Event types: press=`1` (default / may be omitted), repeat=`2`, release=`3`; event is a colon subfield of modifiers. (Kitty protocol, “Key codes,” “Modifiers,” “Event types,” and “Text as code points.”)

Flag 1 converts Escape, Alt+ASCII, Ctrl+ASCII, Ctrl+Alt+ASCII, Shift+Alt+ASCII to CSI-u forms, and makes non-text keypad keys distinct. Ctrl+C therefore becomes `CSI 99;5u` instead of byte `0x03`. **Legacy exceptions with flag 1 alone — only WITHOUT modifiers:** unmodified Enter stays CR (`0x0d`), Tab stays HT (`0x09`), Backspace stays DEL (`0x7f`) or configured legacy backspace byte; with any modifier they use CSI-u codes 13/9/127 (Shift+Enter → `CSI 13;2u`, Ctrl+Enter → `CSI 13;5u`, Alt+Enter → `CSI 13;3u`, Shift+Tab → `CSI 9;2u`, Ctrl+Backspace → `CSI 127;5u`). Verified against kitty `kitty/key_encoding.c` lines 167–220 and `kitty_tests/keys.py` lines 414–422 (shift+enter under flags=0b1 is `csi(shift, 13)`), cross-checked with Ghostty `src/input/key_encode.zig` lines 162–180 (`binding_mods.empty()` guard) — 2026-10-21. Unmodified keys they do not get releases with flag 2 unless flag 8 is also enabled. Letters with Ctrl/Alt are converted to CSI-u when otherwise ambiguous / specified by the protocol: e.g. Alt+a → `CSI 97;3u`, Ctrl+Shift+a → `CSI 97;6u`. Shifted code is not substituted for the primary code (`a` remains 97). Flag 8 reports text-producing and modifier-key events as sequences too, including Enter/Tab/Backspace, and implies disambiguation. (Kitty protocol, “Disambiguate escape codes,” “Report event types,” “Report all keys as escape codes,” “Legacy text keys.”)

### Functional keys

The Kitty table has both CSI-u private-use key codes and legacy-compatible CSI final-byte/tilde forms. When modifiers are present, legacy forms use `CSI 1;modifier X` for final-byte keys, or `CSI number;modifier~` for tilde keys; Kitty's `CSI u` key-code form can represent them canonically instead. For unmodified keys, legacy aliases include SS3 forms in cursor-key mode.

| Key | Kitty functional form(s) |
|---|---|
| Escape / Enter / Tab / Backspace | `CSI 27u` / `CSI 13u` / `CSI 9u` / `CSI 127u` |
| Insert / Delete | `CSI 2~` / `CSI 3~` |
| Up / Down / Right / Left | `CSI 1A` / `CSI 1B` / `CSI 1C` / `CSI 1D` (unmodified abbreviations: `CSI A/B/C/D`; modified: `CSI 1;mod A/B/C/D`) |
| Home / End | `CSI 1H` or `CSI 7~` / `CSI 1F` or `CSI 8~` |
| Page Up / Page Down | `CSI 5~` / `CSI 6~` |
| F1–F4 | `CSI 1P` or `CSI 11~`; `CSI 1Q` or `CSI 12~`; `CSI 13~`; `CSI 1S` or `CSI 14~` |
| F5–F12 | `CSI 15~`, `17~`, `18~`, `19~`, `20~`, `21~`, `23~`, `24~` (with modifiers, prefix number and `;mod`, e.g. `CSI 15;2~`) |
| Common keypad examples | KP_0=`57399u`, KP_1=`57400u`, KP_2=`57401u`, KP_3=`57402u`, KP_Enter=`57414u` |

The functional-key table marks Escape/Enter/Tab/Backspace with their legacy C0/DEL code points; other non-Unicode function keys use PUA codepoints, e.g. CapsLock `57358`, PrintScreen `57361`, Menu `57363`, F13–F35 `57376`–`57398` (F1–F12 have no PUA code in the table, only the CSI forms below — corrected 2026-10-21 against `docs/keyboard-protocol.rst`), keypad keys beginning at `57399`. Functional legacy forms above are the table's `CSI` spellings; SS3 aliases for unmodified arrows/Home/End and F1–F4 are listed in its legacy-encoding table. (Kitty protocol, “Legacy functional encoding” and “Functional key definitions.”)

### Functional key codes (verbatim)

Columns: "CSI-u code" is the "CSI" column of the source "Functional key codes" table (`Functional key definitions`); "Legacy form" is the "Escape code" column of the source "Legacy functional encoding" table, or for Escape/Enter/Tab/Backspace the "No mods" cell of the "C0 controls" table. `-` means the source lists no legacy form for the key (the source says: "kitty chooses to encode these using `CSI u` encoding even in legacy mode"). Note from the source: "The escape codes above of the form `CSI 1 letter` will omit the `1` if there are no modifiers, since `1` is the default value."

| Name | CSI-u code | Legacy form |
|---|---|---|
| ESCAPE | `27 u` | `0x1b` |
| ENTER | `13 u` | `0xd` |
| TAB | `9 u` | `0x9` |
| BACKSPACE | `127 u` | `0x7f` |
| INSERT | `2 ~` | `CSI 2 ~` |
| DELETE | `3 ~` | `CSI 3 ~` |
| LEFT | `1 D` | `CSI D, SS3 D` |
| RIGHT | `1 C` | `CSI C, SS3 C` |
| UP | `1 A` | `CSI A, SS3 A` |
| DOWN | `1 B` | `CSI B, SS3 B` |
| PAGE_UP | `5 ~` | `CSI 5 ~` |
| PAGE_DOWN | `6 ~` | `CSI 6 ~` |
| HOME | `1 H or 7 ~` | `CSI H, SS3 H` |
| END | `1 F or 8 ~` | `CSI F, SS3 F` |
| CAPS_LOCK | `57358 u` | - |
| SCROLL_LOCK | `57359 u` | - |
| NUM_LOCK | `57360 u` | - |
| PRINT_SCREEN | `57361 u` | - |
| PAUSE | `57362 u` | - |
| MENU | `57363 u` | `CSI 29 ~` |
| F1 | `1 P or 11 ~` | `SS3 P` |
| F2 | `1 Q or 12 ~` | `SS3 Q` |
| F3 | `13 ~` | `SS3 R` |
| F4 | `1 S or 14 ~` | `SS3 S` |
| F5 | `15 ~` | `CSI 15 ~` |
| F6 | `17 ~` | `CSI 17 ~` |
| F7 | `18 ~` | `CSI 18 ~` |
| F8 | `19 ~` | `CSI 19 ~` |
| F9 | `20 ~` | `CSI 20 ~` |
| F10 | `21 ~` | `CSI 21 ~` |
| F11 | `23 ~` | `CSI 23 ~` |
| F12 | `24 ~` | `CSI 24 ~` |
| KP_0 | `57399 u` | - |
| KP_1 | `57400 u` | - |
| KP_2 | `57401 u` | - |
| KP_3 | `57402 u` | - |
| KP_4 | `57403 u` | - |
| KP_5 | `57404 u` | - |
| KP_6 | `57405 u` | - |
| KP_7 | `57406 u` | - |
| KP_8 | `57407 u` | - |
| KP_9 | `57408 u` | - |
| KP_DECIMAL | `57409 u` | - |
| KP_DIVIDE | `57410 u` | - |
| KP_MULTIPLY | `57411 u` | - |
| KP_SUBTRACT | `57412 u` | - |
| KP_ADD | `57413 u` | - |
| KP_ENTER | `57414 u` | - |
| KP_EQUAL | `57415 u` | - |
| KP_SEPARATOR | `57416 u` | - |
| KP_LEFT | `57417 u` | - |
| KP_RIGHT | `57418 u` | - |
| KP_UP | `57419 u` | - |
| KP_DOWN | `57420 u` | - |
| KP_PAGE_UP | `57421 u` | - |
| KP_PAGE_DOWN | `57422 u` | - |
| KP_HOME | `57423 u` | - |
| KP_END | `57424 u` | - |
| KP_INSERT | `57425 u` | - |
| KP_DELETE | `57426 u` | - |
| KP_BEGIN | `1 E or 57427 ~` | - |
| LEFT_SHIFT | `57441 u` | - |
| LEFT_CONTROL | `57442 u` | - |
| LEFT_ALT | `57443 u` | - |
| LEFT_SUPER | `57444 u` | - |
| RIGHT_SHIFT | `57447 u` | - |
| RIGHT_CONTROL | `57448 u` | - |
| RIGHT_ALT | `57449 u` | - |
| RIGHT_SUPER | `57450 u` | - |
| ISO_LEVEL3_SHIFT | `57453 u` | - |
| ISO_LEVEL5_SHIFT | `57454 u` | - |

Verbatim rules from the source:

- Modifier keys' own bit ("Modifiers"): "When the key event is related to an actual modifier key, the corresponding modifier's bit must be set to the modifier state including the effect for the current event. For example, when pressing the LEFT_CONTROL key, the `ctrl` bit must be set and when releasing it, it must be reset. When both left and right control keys are pressed and one is released, the release event must have the `ctrl` bit set."
- Keypad, flag 1 ("Disambiguate escape codes"): "Additionally, all non text keypad keys will be reported as separate keys with `CSI u` encoding, using dedicated numbers from the table below." Flags 0 ("Legacy functional keys"): "All keypad keys are reported as their equivalent non-keypad keys. To distinguish these, use the disambiguate flag." (Source flag-1 wording says "non text keypad keys"; it does not spell out the digit/operator keypad keys that produce text.)
- Flag 8 ("Report all keys as escape codes"): "This progressive enhancement (`0b1000`) turns on key reporting even for key events that generate text. When it is enabled, text will not be sent, instead only key events are sent. If the text is needed as well, combine with the Report associated text enhancement below." Also: "Additionally, with this mode, events for pressing modifier keys are reported. Note that *all* keys are reported as escape codes, including Enter, Tab, Backspace etc." (The source has no literal `CSI 97u` example; that form follows from `CSI number; modifier u` with number 97 for `a`: "such as `97` for the `a` key".)
- Flag 16 ("Report associated text"): "This progressive enhancement (`0b10000`) *additionally* causes key events that generate text to be reported as `CSI u` escape codes with the text embedded in the escape code." and "Note that this flag is an enhancement to report_all_keys and is undefined if used without it." ("Text as code points"): "If multiple code points are present, they must be separated by colons. If no known key is associated with the text the key number `0` must be used. The associated text must not contain control codes". Examples: "shift+a -> CSI 97 ; 2 ; 65 u", "alt+a -> CSI 0 ; ; 229 u". The source states no explicit omit rule for the third field; it is only said to be present for key events that generate text.
- SS3 / flag 1 ("Legacy functional keys", "Disambiguate escape codes"): "`CSI number ; modifier ~`, `CSI 1 ; modifier {ABCDEFHPQS}`, `SS3 {ABCDEFHPQRS}`"; "In the above, if there are no modifiers, the modifier parameter is omitted."; "Some keys have an alternate representation when the terminal is in *cursor key mode* (the `smkx/rmkx` terminfo capabilities). This form is used only in *cursor key mode* and only when no modifiers are present." Flag 1: "With this flag turned on, all key events that do not generate text are represented in one of the following two forms: `CSI number; modifier u`, `CSI 1; modifier [~ABCDEFHPQS]`". The source has no sentence that says whether unmodified F1-F4/arrows stay SS3 under flag 1; its flag-1 forms list no SS3 form, and the "CSI 1 letter will omit the 1" note gives `CSI P` / `CSI A` for the unmodified case.

Source: https://sw.kovidgoyal.net/kitty/keyboard-protocol/ (text taken from the page's source `docs/keyboard-protocol.rst`, https://raw.githubusercontent.com/kovidgoyal/kitty/master/docs/keyboard-protocol.rst), fetched 2026-10-21

### Worked byte examples

`CSI` expands to bytes `1b 5b`; e.g. `CSI 97;3u` is `1b 5b 39 37 3b 33 75`.

| Key / active flags | Output bytes / sequence | Why |
|---|---|---|
| Enter (no mods), flag 1 | `0d` | Unmodified Enter remains legacy. |
| Shift+Enter, flag 1 | `CSI 13;2u` | Modified Enter is CSI-u (kitty key_encoding.c, keys.py). |
| Ctrl+Enter, flag 1 | `CSI 13;5u` | Ctrl bit 4 plus offset 1. |
| Alt+a, flag 1 | `CSI 97;3u` | `a`=97; Alt bit 2 plus offset 1. |
| Ctrl+Shift+a, flag 1 | `CSI 97;6u` | Ctrl+Shift bits 4+1 plus offset 1. |
| Escape, flag 1 | `CSI 27u` | Escape is disambiguated. |
| Escape release, flags 1+2 | `CSI 27;1:3u` | Event type 3; no modifiers value 1. |
| Ctrl+c, flag 1 | `CSI 99;5u` | Ctrl bit 4 plus offset 1; avoids ETX. |
| Up, Shift, flag 1 | `CSI 1;2A` | Legacy final-byte form with modifier 2. |
| Enter release, flags 1+2 | no bytes | No release in this mode; flag 8 is needed to report it. |
| Shift+Enter, flag 8 | `CSI 13;2u` | All keys are sequences; Enter's code is 13. |

Examples derive directly from the cited tables and rules; the byte expansion is the ASCII encoding of those sequences.

## 2. Synchronized output (DEC private mode 2026)

**Source:** [Christian Parpart, Terminal Spec: Synchronized Output](https://gist.github.com/christianparpart/d8a62cc1ab659194337d73e399004036); [Contour VT extensions](https://github.com/contour-terminal/vt-extensions) (the gist says the spec moved there); [xterm DECRQM](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html).

* Begin synchronized update: `CSI ? 2026 h` (`ESC [ ? 2 0 2 6 h`).
* End synchronized update / present latest screen state: `CSI ? 2026 l`.
* During the mode, continue processing input and terminal data but defer visible redraw to the last committed frame; ending the mode permits redraw from the latest buffer.
* Query: `CSI ? 2026 $ p`; reply: `CSI ? 2026 ; Ps $ y`.
* `Ps`: `0` unrecognized / unsupported; `1` set; `2` reset (supported but currently off); `3` permanently set (spec says relevance undefined); `4` permanently reset / unsupported.
* Timeout: the gist reports no consensus on whether/how long. It says a too-short timeout is no worse than no synchronized output. **Implementation recommendation (inference from this text):** use a finite watchdog so a client that forgets `CSI ? 2026 l` cannot freeze presentation indefinitely; choose/document the duration locally, since the source gives no normative value.

## 3. DECRQM for DEC private modes

**Source:** [XTerm Control Sequences, DECRQM/DECRPM](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html).

General request: `CSI ? Ps $ p` (DEC private mode `Ps`). Reply: `CSI ? Ps ; Pm $ y`. `Pm` reports: `0` not recognized, `1` set, `2` reset, `3` permanently set, `4` permanently reset. Here `Ps` is a mode number used with DECSET/DECRST. This is distinct from ANSI mode request `CSI Ps $ p` / response `CSI Ps ; Pm $ y` (no `?`).

## 4. OSC 8 hyperlinks

**Source:** [Egmont Koblinger, Hyperlinks in Terminal Emulators](https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda); general OSC framing and terminators: [XTerm Control Sequences](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html).

Open/set: `OSC 8 ; params ; URI ST` (`1b 5d 38 3b ... 3b ... 1b 5c`). Following painted cells link to the URI. Close: `OSC 8 ; ; ST` (retain both semicolons, empty params and URI). `params` is colon-separated key/value data; the lowercase `id=...` parameter connects cells for hover treatment. Cells are connected only when both their URI and nonempty `id` match; a shared id must not join cells pointing to different URIs. (The gist does not prescribe a global id allocation scheme.)

URI is URI-encoded. Suggested schemes include `http:`, `https:`, `ftp:`, `file:`, and `mailto:`; support and launch policy belong to the terminal. A terminal may whitelist schemes or ask before launching less familiar ones. Do not treat arbitrary URI data as safe to launch.

ST is the specified terminator. Generic xterm OSC accepts BEL or ST and mirrors the query terminator in replies, but the OSC 8 gist specifies ST and does not establish BEL as OSC 8 framing; prefer ST for OSC 8. (The source says URI limit is implementation-defined: VTE and iTerm2 use 2083 bytes; de facto limit is about 2000 bytes. It also notes terminals may cap the whole OSC 8 sequence. No universal max is specified.)

## 5. OSC 52 clipboard / selection

**Source:** [XTerm Control Sequences, OSC 52](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html), [OSC 52 explanation / XTerm](https://invisible-island.net/xterm/xterm-paste64.html).

Format: `OSC 52 ; Pc ; Pd ST` (xterm accepts BEL or ST). `Pc` is a sequence of selection identifiers: `c` clipboard, `p` primary, `s` secondary, `0`–`7` cut buffers. If empty, xterm uses `s0` (configured primary/clipboard selection and cut-buffer 0); if several are supplied, query searches in order and returns the first available selection. `Pd` is RFC 4648 Base64 data to replace the selected data. `Pd=?` requests data; reply uses the same OSC form with returned Base64 data. If `Pd` is neither Base64 nor `?`, xterm clears the selection. **Inference:** clearing commonly uses an empty payload, but xterm's text says invalid/non-query data clears, without mandating one canonical empty spelling. OSC 52 can be disabled by implementation policy; xterm documents gating it with `allowWindowOps`.

## 6. OSC 7 working directory

**Sources:** [Kitty shell integration](https://sw.kovidgoyal.net/kitty/shell-integration/) (reports current working directory); [Contour release notes/source](https://github.com/contour-terminal/contour) (documents file URL, hostname, percent-encoded path); [OSC 7 format reference](https://www.terminfo.dev/extensions/osc-7-current-directory) (secondary format reference).

Format: `OSC 7 ; file://host/path ST`, e.g. `ESC ] 7 ; file://localhost/home/me/work ESC \`. Path is a URI path; percent-encode bytes that are not legal raw URI path data (notably spaces, `%`, and control bytes), so path data cannot inject terminal controls. Contour's shell integration uses a `file://` URL with host name and percent-encoded path, and decodes the URL path when consuming it.

**Host check:** the local/remote hostname distinction is important when a shell is reached over SSH. The source material confirms a hostname is carried, but does not give one universal acceptance algorithm. **Inference:** consumers should verify that host is local (or explicitly map a trusted remote host) before using the path to open a local directory; never silently interpret a remote path as local.

## 7. OSC color set/query/reset formats

**Source:** [XTerm Control Sequences, Operating System Commands](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html).

OSC query/set framing: `OSC Ps ; ... ST` (xterm also accepts BEL). For query, use `?`; xterm's reply has the corresponding set form. **Terminator:** xterm uses the same terminator used by the query in its reply; generic OSC docs prefer ST, with BEL retained for compatibility.

| OSC | Query | Set reply payload form | Reset |
|---|---|---|---|
| `4` palette index | `OSC 4 ; index ; ? ST` | `OSC 4 ; index ; rgb:rrrr/gggg/bbbb ST` (one reply per queried index) | `OSC 104` resets all palette entries; `OSC 104 ; index` resets listed entries. |
| `10` foreground | `OSC 10 ; ? ST` | `OSC 10 ; rgb:rrrr/gggg/bbbb ST` | `OSC 110` |
| `11` background | `OSC 11 ; ? ST` | `OSC 11 ; rgb:rrrr/gggg/bbbb ST` | `OSC 111` |
| `12` text cursor color | `OSC 12 ; ? ST` | `OSC 12 ; rgb:rrrr/gggg/bbbb ST` | `OSC 112` |

The RGB payload syntax is `rgb:` followed by three slash-separated hex channels. The number of hex digits per channel may vary; `rrrr` above illustrates a 16-bit channel representation, not a mandated fixed width. OSC 4 indexes address ANSI colors 0–15 and, where supported, extended palette entries. OSC 104 without indexes resets the entire palette. OSC 110/111/112 reset the corresponding dynamic colors to configured defaults. **Inference:** a parser should accept varying channel widths rather than require exactly four hex digits; XParseColor RGB specifications are used by xterm.

## 8. DA1, DA2, and XTVERSION

**Source:** [XTerm Control Sequences, Device Attributes and XTVERSION](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html).

* DA1 request: `CSI c` (or `CSI 0 c`); primary attributes reply is terminal-specific, commonly `CSI ? ... c`.
* DA2 request: `CSI > c` (or `CSI > 0 c`); reply format: `CSI > Pp ; Pv ; Pc c`. `Pp` identifies terminal type, `Pv` version, `Pc` hardware/configuration parameter(s); values are implementation-defined. xterm documents examples/type values in its DA section.
* XTVERSION request: `CSI > q` (xterm also documents `CSI > 0 q`); reply: `DCS > | name(version) ST`, where text identifies terminal and version. DCS framing uses `ESC P` ... `ESC \` in 7-bit form.

## 9. SGR attributes, underline styles, and color

**Sources:** [XTerm Control Sequences, SGR](https://invisible-island.net/xterm/ctlseqs/ctlseqs.html); [Alacritty changelog](https://github.com/alacritty/alacritty/blob/master/CHANGELOG.md) for underline-style and underline-color implementation formats.

| SGR | Effect |
|---|---|
| `3` / `23` | italic on / italic off |
| `9` / `29` | strike-through on / off |
| `4` | single underline |
| `4:0` | underline off; `4:1` single; `4:2` double; `4:3` curly; `4:4` dotted; `4:5` dashed |
| `21` | double underline (ECMA-48 3rd); **compatibility caveat (inference):** some terminals historically use it as bold-off, so check target behavior (the cited xterm and ECMA-48 references define double underline) |
| `24` | underline off |
| `58;5;n` | indexed underline color `n` |
| `58;2;r;g;b` | RGB underline color (legacy semicolon form) |
| `58:5:n` | indexed underline color (colon form) |
| `58:2::r:g:b` | RGB underline color, with empty color-space subparameter |
| `59` | reset underline color to default |
| `8` / `28` | hidden/invisible on / visible (hidden off) |
| `22` | turn off both bold and faint/dim |
| `53` / `55` | overline on / off |

`CSI ... m` is the SGR envelope. Colon is the ISO-8613-6 subparameter separator, used to keep a compound attribute such as underline style or color within one SGR parameter. Semicolon is the outer SGR parameter separator and legacy color syntax (`58;2;r;g;b`) consumes following semicolon parameters as components. A parser must not treat `4:3` as separate SGR commands `4` and `3`; likewise the empty field in `58:2::r:g:b` is significant. Xterm states colon is standard for indexed/direct color and semicolon is legacy; after a colon begins a subparameter sequence, continue using colons. (Xterm's published SGR table confirms `3/23`, `8/28`, `9/29`, `21/22/24`; [ECMA-48, 5th edition](https://ecma-international.org/wp-content/uploads/ECMA-48_5th_edition_june_1991.pdf) defines `53/55` and `21`; Alacritty changelog confirms `4:0`, `4:2`, `4:3..5`, `58:2`, `58:5`, and `59`.)
