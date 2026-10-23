use crate::input::TerminalInteraction;
use crate::launch::TerminalLaunchSpec;
use crate::terminal_process::{
    HostChannels, PendingHostEvents, SessionSink, notify_main_loop, terminal_reply_channel,
};
use crate::wake::WakeHandle;
use ronsole_term::{Term, TermConfig, TermIdentity, TermPalette};
use sha2::{Digest, Sha256};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) use ronsole_term::{
    CELL_PRESENTATION_EMOJI, CELL_PRESENTATION_TEXT, Cell, MouseTrackingMode, TerminalColor,
    normalized_selection_bounds, terminal_should_render_zero_width,
};

/// Scrollback capacity of every Ronsole terminal grid.
const TERMINAL_SCROLLBACK_LINES: usize = 10_000;

/// Builds the terminal model with Ronsole's identity and the given palette.
fn new_term(cols: usize, rows: usize, palette: TermPalette) -> Term {
    Term::new(TermConfig {
        cols,
        rows,
        scrollback_lines: TERMINAL_SCROLLBACK_LINES,
        identity: TermIdentity {
            name: "ronsole",
            version: env!("CARGO_PKG_VERSION"),
        },
        palette,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terminal model with the default background palette, as
    /// `Terminal::new_for_test` builds it.
    fn test_term(cols: usize, rows: usize) -> Term {
        new_term(
            cols,
            rows,
            crate::renderer::terminal_palette(crate::config::DEFAULT_TERMINAL_BACKGROUND),
        )
    }

    /// Feeds `bytes` through `advance` as one output batch; replies and
    /// titles are discarded.
    fn feed(term: &mut Term, bytes: &[u8]) {
        let mut sink = SessionSink {
            reply_tx: None,
            title_cache: None,
            pending: None,
            wake: None,
        };
        term.advance(bytes, Instant::now(), &mut sink);
    }

    /// Scrollback rows, oldest first.
    fn scrollback_rows(term: &Term) -> impl Iterator<Item = &[Cell]> + '_ {
        (0..term.scrollback_len()).filter_map(move |index| term.row(index))
    }

    /// Visible screen rows, top first.
    fn screen_rows(term: &Term) -> impl Iterator<Item = &[Cell]> + '_ {
        (0..term.rows()).filter_map(move |y| term.visible_row(y))
    }

    #[test]
    fn terminal_spawn_error_is_ready_and_keeps_error_text_visible() {
        let title_cache = Arc::new(Mutex::new(
            crate::terminal_process::TerminalTitleState::new("terminal error".to_string()),
        ));
        let mut grid = test_term(48, 3);
        write_terminal_spawn_error(&mut grid, &io::Error::other("spawn failed"));
        let grid = Arc::new(Mutex::new(grid));
        let mut terminal = Terminal {
            grid: grid.clone(),
            bridge_screen_lock: Arc::new(Mutex::new(())),
            process: None,
            hold: false,
            scroll_y: crate::scroll::ScrollState::new(7.0),
            presentation_intent: TerminalPresentationIntent::None,
            reveal_right_tail_when_presented: false,
            title_cache,
            tab_token: crate::terminal_process::TabToken::generate(),
            last_user_input: None,
            user_input_held: false,
            held_user_input: Vec::new(),
            bridge_launch: None,
            dev_warning: DevWarningState::default(),
            dev_banner: DevBannerState::default(),
            osc7_cwd: None,
            reply_tx: terminal_reply_channel().0,
            pending: Arc::new(Mutex::new(PendingHostEvents::default())),
            wake: None,
            test_input: None,
        };

        assert!(!terminal.is_closed());
        let mut grid = crate::platform::lock_recover(&grid);
        assert!(grid.presentation_ready());
        assert!(!grid.presentation_visible());
        grid.mark_presentation_layout_ready();
        assert!(grid.presentation_visible());
        let text = (0..grid.rows())
            .filter_map(|y| grid.visible_row(y))
            .flat_map(|line| line.iter().map(|cell| cell.ch()))
            .collect::<String>();
        assert!(text.contains("Ronsole terminal error: spawn failed"));
    }

    #[test]
    fn full_terminal_reply_queue_never_blocks_parser_and_stays_bounded() {
        const CAPACITY: usize = 2;
        const QUERY_BATCHES: usize = 10_000;
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(CAPACITY);
        reply_tx.try_send(vec![1]).unwrap();
        reply_tx.try_send(vec![2]).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);

        let worker = std::thread::spawn(move || {
            let mut grid = test_term(8, 3);
            let mut sink = SessionSink {
                reply_tx: Some(&reply_tx),
                title_cache: None,
                pending: None,
                wake: None,
            };
            for _ in 0..QUERY_BATCHES {
                grid.advance(b"\x1b[6n\x1b[c\x1b]10;?\x1b\\", Instant::now(), &mut sink);
            }
            done_tx.send(()).unwrap();
        });

        done_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("full reply queue blocked terminal parser");
        worker.join().unwrap();
        assert_eq!(reply_rx.try_iter().count(), CAPACITY);
    }

    #[test]
    fn pathological_combining_cluster_is_bounded_and_safe_to_reconstruct_and_search() {
        let mut grid = test_term(8, 1);
        let mut bytes = b"A".to_vec();
        for _ in 0..100_000 {
            bytes.extend_from_slice("\u{0301}".as_bytes());
        }
        feed(&mut grid, &bytes);
        assert_eq!(
            grid.visible_row(0).expect("screen row")[0].zero_width().len(),
            ronsole_term::TERMINAL_CELL_EXTRA_MAX_CHARS
        );

        grid.set_selection(Some((0, 0, 0, 0)));
        let selected = grid.selection_text();
        assert_eq!(
            selected.chars().count(),
            1 + ronsole_term::TERMINAL_CELL_EXTRA_MAX_CHARS
        );

        grid.mark_changed();
        let mut search = crate::search::TerminalSearchState::default();
        search.open();
        search.insert_text("A\u{0301}");
        assert!(search.recompute_if_needed(
            &grid,
            crate::search::SearchRefreshCause::User
        ));
        assert_eq!(search.results.len(), 1);
        assert_eq!(search.results[0].start_x, 0);
    }

    #[test]
    fn real_pty_shell_emits_output_and_observes_resize() {
        use std::time::{Duration, Instant};

        let mut terminal = Terminal::spawn(
            None,
            1,
            TerminalLaunchSpec::hermetic_shell_for_test(),
            crate::renderer::terminal_palette(crate::config::DEFAULT_TERMINAL_BACKGROUND),
        );
        terminal.resize_pty(120, 40);
        terminal
            .write_input(b"printf '__RONS_PTY__\\n'; stty size; exit\r")
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(3);
        let text = loop {
            let text = {
                let grid = crate::platform::lock_recover(&terminal.grid);
                (0..grid.total_rows())
                    .filter_map(|index| grid.row(index))
                    .flat_map(|line| line.iter().map(|cell| cell.ch()))
                    .collect::<String>()
            };
            if text.contains("__RONS_PTY__") && text.contains("40 120") {
                break text;
            }
            assert!(Instant::now() < deadline, "PTY output did not reach the terminal grid: {text:?}");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(text.contains("__RONS_PTY__"));
        assert!(text.contains("40 120"));

        let deadline = Instant::now() + Duration::from_secs(3);
        while !terminal.is_closed() {
            assert!(Instant::now() < deadline, "shell did not exit after the scripted command");
            std::thread::sleep(Duration::from_millis(10));
        }
        terminal.shutdown();
    }

    #[test]
    fn tabs_get_distinct_tab_tokens() {
        let first = Terminal::new_for_test(10, 4, 1);
        let second = Terminal::new_for_test(10, 4, 2);
        assert_eq!(first.tab_token().len(), 32);
        assert_ne!(first.tab_token(), second.tab_token());
    }

    #[test]
    fn screen_tail_text_reads_current_screen_including_alt_screen() {
        let terminal = Terminal::new_for_test(10, 4, 1);
        {
            let mut grid = crate::platform::lock_recover(&terminal.grid);
            feed(&mut grid, b"zero\r\none\r\ntwo  \r\nthree");
        }
        assert_eq!(terminal.screen_tail_text(2), ["two", "three"]);
        assert_eq!(terminal.screen_tail_text(9), ["zero", "one", "two", "three"]);
        assert!(terminal.screen_tail_text(0).is_empty());

        {
            let mut grid = crate::platform::lock_recover(&terminal.grid);
            feed(&mut grid, b"\x1b[?1049h\x1b[Halt1\r\nalt2");
            assert!(grid.is_alt_screen());
        }
        assert_eq!(terminal.screen_tail_text(4), ["alt1", "alt2", "", ""]);
    }

    #[test]
    fn bridge_warning_requires_all_fragments_and_survives_wrapped_rows() {
        let screen = "  WARNING: Loading development channels\n\n  --dangerously-load-development-channels is for local channel development only.\n\n  ❯ 1. I am using this for local development\n    2. Exit\n";
        assert!(bridge_warning_visible(screen));

        let characters: Vec<char> = screen.chars().collect();
        let wrapped = characters
            .chunks(40)
            .map(|chunk| chunk.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(bridge_warning_visible(&wrapped));

        assert!(!bridge_warning_visible(
            "Loading development channels --dangerously-load-development-channels"
        ));
    }

    #[test]
    fn bridge_enter_is_bridge_only_single_use_and_expires_after_fifteen_minutes() {
        let now = Instant::now();
        let warning = "Loading development channels --dangerously-load-development-channels ❯ 1. I am using this for local development";
        let mut bridge_tab = BridgeLaunchState::new([7; 16], now);
        let bridge_without_warning = BridgeLaunchState::new([6; 16], now);
        assert!(!bridge_without_warning.should_auto_confirm(now, "ordinary Claude prompt"));
        assert!(bridge_tab.should_auto_confirm(now, warning));
        bridge_tab.enter_sent = true;
        assert!(!bridge_tab.should_auto_confirm(now, warning));

        let expired = BridgeLaunchState::new([8; 16], now);
        assert!(!expired.should_auto_confirm(now + BRIDGE_LAUNCH_TTL, warning));
        assert!(!expired.enter_sent);
    }

    #[test]
    fn ordinary_tab_does_not_auto_confirm_bridge_warning() {
        let now = Instant::now();
        let mut terminal = Terminal::new_for_test(120, 8, 1);
        feed_bridge_warning(&terminal, true);

        assert!(!terminal.maybe_auto_confirm_bridge(now));
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    #[test]
    fn bridge_tab_auto_confirms_warning_once_through_input_writer() {
        let now = Instant::now();
        let mut terminal = Terminal::new_for_test(120, 8, 1);
        terminal.bridge_launch = Some(BridgeLaunchState::new([9; 16], now));
        feed_bridge_warning(&terminal, true);

        assert!(terminal.maybe_auto_confirm_bridge(now));
        assert!(!terminal.maybe_auto_confirm_bridge(now));
        assert_eq!(terminal.input_bytes_for_test(), b"\r");
    }

    #[test]
    fn bridge_tab_does_not_auto_confirm_without_warning() {
        let now = Instant::now();
        let mut terminal = Terminal::new_for_test(120, 8, 1);
        terminal.bridge_launch = Some(BridgeLaunchState::new([10; 16], now));
        feed_bridge_warning(&terminal, false);

        assert!(!terminal.maybe_auto_confirm_bridge(now));
        assert!(terminal.input_bytes_for_test().is_empty());
    }

    #[test]
    fn manual_bridge_confirm_works_without_warning_and_failed_writes_remain_pending() {
        let now = Instant::now();
        let mut terminal = Terminal::new_for_test(80, 8, 1);
        terminal.bridge_launch = Some(BridgeLaunchState::new([11; 16], now));
        feed(
            &mut crate::platform::lock_recover(&terminal.grid),
            b"ordinary screen",
        );
        let screen = terminal.screen_tail_text(usize::MAX).join("\n");

        assert!(terminal.manually_confirm_bridge(now, screen_hash(&screen)));
        assert!(!terminal.manually_confirm_bridge(now, screen_hash(&screen)));
        assert_eq!(terminal.input_bytes_for_test(), b"\r");

        let mut failed = Terminal::new_for_test(80, 8, 1);
        failed.bridge_launch = Some(BridgeLaunchState::new([12; 16], now));
        feed(
            &mut crate::platform::lock_recover(&failed.grid),
            b"ordinary screen",
        );
        let screen = failed.screen_tail_text(usize::MAX).join("\n");
        failed.test_input = None;
        assert!(!failed.manually_confirm_bridge(now, screen_hash(&screen)));
        assert!(!failed.bridge_launch_state().unwrap().enter_sent);

        let mut auto = Terminal::new_for_test(120, 8, 1);
        auto.bridge_launch = Some(BridgeLaunchState::new([13; 16], now));
        feed_bridge_warning(&auto, true);
        auto.test_input = None;
        assert!(!auto.maybe_auto_confirm_bridge(now));
        assert!(!auto.bridge_launch_state().unwrap().enter_sent);
    }

    fn feed_bridge_warning(terminal: &Terminal, include_choice: bool) {
        let screen = if include_choice {
            "WARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 1. I am using this for local development"
        } else {
            "WARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 2. Exit"
        };
        let mut grid = crate::platform::lock_recover(&terminal.grid);
        feed(&mut grid, screen.as_bytes());
    }

    #[test]
    fn manual_enter_marks_bridge_state_as_confirmed() {
        let now = Instant::now();
        let mut terminal = Terminal::new_for_test(10, 4, 1);
        terminal.bridge_launch = Some(BridgeLaunchState::new([3; 16], now));
        terminal.write_user_input(b"\r");
        assert!(terminal.bridge_launch_state().unwrap().enter_sent);
    }

    const DEV_WARNING_SCREEN: &str = "\x1b[2J\x1b[HWARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 1. I am using this for local development";
    const DEV_CLAUDE: crate::tab_input::ProcGen = crate::tab_input::ProcGen {
        pid: 42,
        starttime: 7,
    };

    /// Feeds `bytes` as one PTY output batch (bumps `content_generation`).
    fn feed_dev_screen(terminal: &Terminal, bytes: &str) {
        let mut grid = crate::platform::lock_recover(&terminal.grid);
        feed(&mut grid, bytes.as_bytes());
        grid.mark_changed();
    }

    fn observe_dev(
        terminal: &mut Terminal,
        now: Instant,
        claude: Option<crate::tab_input::ProcGen>,
    ) -> bool {
        let mut scratch = String::new();
        terminal.observe_dev_channel_warning(now, &mut scratch, |_| claude)
    }

    #[test]
    fn dev_warning_sends_one_enter_per_flagged_claude_generation() {
        let now = Instant::now();
        let mut terminal = Terminal::new_for_test(120, 8, 1);
        feed_dev_screen(&terminal, DEV_WARNING_SCREEN);
        assert!(observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        assert_eq!(terminal.input_bytes_for_test(), b"\r");

        // Same frame again and a repainted warning frame: no second Enter.
        assert!(!observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        feed_dev_screen(&terminal, DEV_WARNING_SCREEN);
        assert!(!observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        assert_eq!(terminal.input_bytes_for_test(), b"\r");

        // The same claude showing the warning again later is not confirmed twice.
        feed_dev_screen(&terminal, "\x1b[2J\x1b[H> ready");
        assert!(!observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        feed_dev_screen(&terminal, DEV_WARNING_SCREEN);
        assert!(!observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        assert!(!terminal.dev_warning_frame_hidden(now));
        assert_eq!(terminal.input_bytes_for_test(), b"\r");

        // A restarted claude (new start time) gets its own Enter.
        feed_dev_screen(&terminal, "\x1b[2J\x1b[H$ ");
        assert!(!observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        feed_dev_screen(&terminal, DEV_WARNING_SCREEN);
        let restarted = crate::tab_input::ProcGen {
            pid: 42,
            starttime: 8,
        };
        assert!(observe_dev(&mut terminal, now, Some(restarted)));
        assert_eq!(terminal.input_bytes_for_test(), b"\r\r");
    }

    #[test]
    fn dev_warning_needs_flagged_claude_and_the_exact_warning() {
        let now = Instant::now();
        // No flagged claude in the foreground group (other program, claude
        // without the flag): nothing is sent and nothing is hidden.
        let mut terminal = Terminal::new_for_test(120, 8, 1);
        feed_dev_screen(&terminal, DEV_WARNING_SCREEN);
        assert!(!observe_dev(&mut terminal, now, None));
        assert!(!terminal.dev_warning_frame_hidden(now));
        assert!(terminal.input_bytes_for_test().is_empty());

        // Changed text never reaches the process lookup.
        for screen in [
            "\x1b[2J\x1b[HWARNING: Loading development channels\r\n--dangerously-load-development-channels\r\n❯ 2. Exit",
            "\x1b[2J\x1b[HLoading development channels\r\n❯ 1. I am using this for local development",
            "\x1b[2J\x1b[Hordinary claude prompt",
        ] {
            let mut terminal = Terminal::new_for_test(120, 8, 1);
            feed_dev_screen(&terminal, screen);
            let mut scratch = String::new();
            assert!(!terminal.observe_dev_channel_warning(now, &mut scratch, |_| {
                panic!("process lookup must wait for a matching screen")
            }));
            assert!(terminal.input_bytes_for_test().is_empty());
        }

        // A failed write leaves the warning unconfirmed and visible.
        let mut failed = Terminal::new_for_test(120, 8, 1);
        failed.test_input = None;
        feed_dev_screen(&failed, DEV_WARNING_SCREEN);
        assert!(!observe_dev(&mut failed, now, Some(DEV_CLAUDE)));
        assert!(!failed.dev_warning_frame_hidden(now));
    }

    #[test]
    fn dev_warning_frames_stay_hidden_until_screen_changes_or_limit() {
        let now = Instant::now();
        let mut terminal = Terminal::new_for_test(120, 8, 1);
        feed_dev_screen(&terminal, "\x1b[2J\x1b[H$ claude");
        assert!(!observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        assert!(!terminal.dev_warning_frame_hidden(now));

        feed_dev_screen(&terminal, DEV_WARNING_SCREEN);
        assert!(observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        assert_eq!(
            terminal.dev_warning_hidden_until(),
            Some(now + DEV_WARNING_HIDE_LIMIT)
        );
        assert!(terminal.dev_warning_frame_hidden(now));
        let almost = now + DEV_WARNING_HIDE_LIMIT - Duration::from_millis(1);
        assert!(terminal.dev_warning_frame_hidden(almost));
        // Safety limit: the stuck warning is shown so the user can answer.
        assert!(!terminal.dev_warning_frame_hidden(now + DEV_WARNING_HIDE_LIMIT));

        // The first frame without the warning ends hiding.
        let mut terminal = Terminal::new_for_test(120, 8, 1);
        feed_dev_screen(&terminal, DEV_WARNING_SCREEN);
        assert!(observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        feed_dev_screen(&terminal, "\x1b[2J\x1b[H> ready");
        assert!(!observe_dev(&mut terminal, now, Some(DEV_CLAUDE)));
        assert!(!terminal.dev_warning_frame_hidden(now));
        assert_eq!(terminal.dev_warning_hidden_until(), None);
    }

    #[test]
    fn dev_warning_and_bridge_auto_confirm_send_one_enter_together() {
        let now = Instant::now();
        let mut generic_first = Terminal::new_for_test(120, 8, 1);
        generic_first.bridge_launch = Some(BridgeLaunchState::new([21; 16], now));
        feed_dev_screen(&generic_first, DEV_WARNING_SCREEN);
        assert!(observe_dev(&mut generic_first, now, Some(DEV_CLAUDE)));
        assert!(generic_first.bridge_launch_state().unwrap().enter_sent);
        assert!(!generic_first.maybe_auto_confirm_bridge(now));
        assert_eq!(generic_first.input_bytes_for_test(), b"\r");

        let mut bridge_first = Terminal::new_for_test(120, 8, 1);
        bridge_first.bridge_launch = Some(BridgeLaunchState::new([22; 16], now));
        feed_dev_screen(&bridge_first, DEV_WARNING_SCREEN);
        assert!(bridge_first.maybe_auto_confirm_bridge(now));
        assert!(!observe_dev(&mut bridge_first, now, Some(DEV_CLAUDE)));
        assert!(bridge_first.dev_warning_frame_hidden(now));
        assert_eq!(bridge_first.input_bytes_for_test(), b"\r");
    }

    /// Word wrap like Claude Code's notice box: rows of at most `width`
    /// columns, the space at a break dropped, overlong words split.
    fn notice_wrap(text: &str, width: usize) -> Vec<String> {
        let mut rows = Vec::new();
        let mut row = String::new();
        for mut word in text.split(' ') {
            loop {
                let used = row.chars().count();
                if used + usize::from(used > 0) + word.chars().count() <= width {
                    if used > 0 {
                        row.push(' ');
                    }
                    row.push_str(word);
                    break;
                }
                if used > 0 {
                    rows.push(std::mem::take(&mut row));
                    continue;
                }
                let head: String = word.chars().take(width).collect();
                word = &word[head.len()..];
                rows.push(head);
            }
        }
        rows.push(row);
        rows
    }

    /// Screen with "above", both banner lines wrapped to `width` text columns
    /// behind the quote bar, and "below".
    fn banner_screen(lines: [&str; 2], width: usize) -> (String, usize) {
        let rows: Vec<String> = lines
            .iter()
            .flat_map(|line| notice_wrap(line, width))
            .map(|row| format!("\u{258e} {row}"))
            .collect();
        (
            format!("\x1b[2J\x1b[Habove\r\n{}\r\nbelow \u{258e} x", rows.join("\r\n")),
            rows.len(),
        )
    }

    /// (hidden, non-blank) cell counts of screen row `row`.
    fn hidden_cells(terminal: &Terminal, row: usize) -> (usize, usize) {
        let grid = crate::platform::lock_recover(&terminal.grid);
        let cells = grid.visible_row(row).expect("screen row").iter().filter(|cell| cell.ch() != ' ');
        let (hidden, total) = cells.fold((0, 0), |(hidden, total), cell| {
            (hidden + usize::from(cell.is_hidden()), total + 1)
        });
        (hidden, total)
    }

    fn assert_banner_rows(terminal: &Terminal, banner_rows: usize, hidden: bool) {
        for row in 1..=banner_rows {
            let (hidden_count, total) = hidden_cells(terminal, row);
            assert!(total > 0);
            assert_eq!(hidden_count, if hidden { total } else { 0 }, "row {row}");
        }
        for row in [0, banner_rows + 1] {
            assert_eq!(hidden_cells(terminal, row).0, 0, "neighbour row {row}");
        }
    }

    #[test]
    fn dev_banner_rows_are_hidden_for_flagged_claude_only() {
        let mut terminal = Terminal::new_for_test(200, 8, 1);
        let (screen, rows) = banner_screen(DEV_CHANNEL_BANNER_LINES, 190);
        assert_eq!(rows, 2);
        feed_dev_screen(&terminal, &screen);
        let cursor = {
            let grid = crate::platform::lock_recover(&terminal.grid);
            (grid.cursor(), grid.content_generation())
        };
        terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
        assert_banner_rows(&terminal, rows, true);
        // Only the draw flag changed: cursor, generation and text are intact.
        {
            let grid = crate::platform::lock_recover(&terminal.grid);
            assert_eq!((grid.cursor(), grid.content_generation()), cursor);
        }
        let text = terminal.screen_tail_text(usize::MAX);
        assert_eq!(text[1], format!("\u{258e} {}", DEV_CHANNEL_BANNER_LINES[0]));
        assert_eq!(text[2], format!("\u{258e} {}", DEV_CHANNEL_BANNER_LINES[1]));

        // Another process in the tab: the same text stays visible.
        let mut other = Terminal::new_for_test(200, 8, 1);
        feed_dev_screen(&other, &screen);
        other.observe_dev_channel_banner(|_| None);
        assert_banner_rows(&other, rows, false);
    }

    #[test]
    fn dev_banner_with_changed_text_is_not_hidden() {
        let first = DEV_CHANNEL_BANNER_LINES[0];
        let second = DEV_CHANNEL_BANNER_LINES[1];
        let changed_first = first.replace("server:cc-tg", "server:cc-tx");
        let changed_second = format!("{second}s");
        for (lines, screen) in [
            ([changed_first.as_str(), second], None),
            ([first, changed_second.as_str()], None),
            ([first, "server:cc-tg"], None),
            ([second, first], None),
            // The first line alone, then a prompt.
            ([first, second], Some(format!("\x1b[2J\x1b[Habove\r\n\u{258e} {first}\r\n> {second}"))),
            // No quote bar.
            ([first, second], Some(format!("\x1b[2J\x1b[Habove\r\n  {first}\r\n  {second}"))),
        ] {
            let mut terminal = Terminal::new_for_test(200, 8, 1);
            let screen = screen.unwrap_or_else(|| banner_screen(lines, 190).0);
            feed_dev_screen(&terminal, &screen);
            terminal.observe_dev_channel_banner(|_| {
                panic!("process lookup must wait for the exact banner")
            });
            for row in 0..4 {
                assert_eq!(hidden_cells(&terminal, row).0, 0, "{screen:?}");
            }
        }
    }

    #[test]
    fn dev_banner_wrapped_on_narrow_width_is_hidden() {
        let mut terminal = Terminal::new_for_test(40, 12, 1);
        let (screen, rows) = banner_screen(DEV_CHANNEL_BANNER_LINES, 38);
        // The first line wraps over several rows, the flag word is split.
        assert!(rows >= 6, "{screen:?}");
        assert!(!screen.contains("--dangerously-load-development-channels"));
        feed_dev_screen(&terminal, &screen);
        terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
        assert_banner_rows(&terminal, rows, true);
    }

    #[test]
    fn dev_banner_in_chat_pane_is_hidden_without_hiding_side_panel() {
        let mut terminal = Terminal::new_for_test(143, 8, 1);
        let first = DEV_CHANNEL_BANNER_LINES[0];
        let split = 77;
        let first_head: String = first.chars().take(split).collect();
        let first_tail: String = first.chars().skip(split).collect();
        let screen = format!(
            "\x1b[2J\x1b[H▎ {first_head}\x1b[80G│ panel alpha\x1b[2;1H▎ {first_tail}\x1b[80G│ panel beta\x1b[3;1H▎ {}\x1b[80G│ panel gamma",
            DEV_CHANNEL_BANNER_LINES[1]
        );
        feed_dev_screen(&terminal, &screen);
        terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));

        let grid = crate::platform::lock_recover(&terminal.grid);
        for row in 0..3 {
            let line = grid.visible_row(row).expect("screen row");
            assert!(line[..79]
                .iter()
                .filter(|cell| cell.ch() != ' ')
                .all(|cell| cell.is_hidden()), "banner row {row}");
            assert_eq!(line[79].ch(), '│');
            assert!(!line[79].is_hidden(), "divider row {row}");
            assert_eq!(line[81].ch(), 'p');
            assert!(!line[81].is_hidden(), "panel row {row}");
        }
    }

    #[test]
    fn dev_banner_rewrite_reveals_changed_rows_and_reuses_the_lookup() {
        let lookups = std::cell::Cell::new(0);
        let find = |_| {
            lookups.set(lookups.get() + 1);
            Some(DEV_CLAUDE)
        };
        let mut terminal = Terminal::new_for_test(200, 8, 1);
        let (screen, rows) = banner_screen(DEV_CHANNEL_BANNER_LINES, 190);
        feed_dev_screen(&terminal, &screen);
        terminal.observe_dev_channel_banner(find);
        assert_banner_rows(&terminal, rows, true);

        // Output elsewhere keeps the block hidden without a new lookup.
        feed_dev_screen(&terminal, "\x1b[6;1Hmore output");
        terminal.observe_dev_channel_banner(find);
        assert_banner_rows(&terminal, rows, true);
        assert_eq!(hidden_cells(&terminal, 5).0, 0);

        // The same text written again is hidden with the cached answer.
        feed_dev_screen(&terminal, "\x1b[2;3HC");
        assert_eq!(hidden_cells(&terminal, 1).0 + 1, hidden_cells(&terminal, 1).1);
        terminal.observe_dev_channel_banner(find);
        assert_banner_rows(&terminal, rows, true);
        assert_eq!(lookups.get(), 1);

        // One changed character: the block no longer matches and is shown.
        feed_dev_screen(&terminal, "\x1b[2;3HX");
        terminal.observe_dev_channel_banner(find);
        assert_banner_rows(&terminal, rows, false);
        feed_dev_screen(&terminal, "\x1b[2;3HC");
        terminal.observe_dev_channel_banner(find);
        assert_banner_rows(&terminal, rows, true);
        assert_eq!(lookups.get(), 2);

        // Scrolled into scrollback, the hidden cells keep their flag.
        feed_dev_screen(&terminal, "\x1b[8;1H\r\n\r\n\r\n\r\n\r\n\r\n\r\n");
        terminal.observe_dev_channel_banner(find);
        let grid = crate::platform::lock_recover(&terminal.grid);
        let hidden_in_scrollback = scrollback_rows(&grid)
            .flatten()
            .filter(|cell| cell.is_hidden())
            .count();
        assert!(hidden_in_scrollback > 0);
    }

    /// Asserts over every scrollback and screen row that the rows starting
    /// with "above"/"below" and blank rows have no hidden cell, and every
    /// other row is fully hidden: `banner_rows` rows (when given) that hold
    /// exactly the text of both banner lines.
    fn assert_only_banner_hidden(terminal: &Terminal, banner_rows: Option<usize>) {
        let grid = crate::platform::lock_recover(&terminal.grid);
        let mut hidden_rows = 0;
        let mut hidden_text = String::new();
        for line in (0..grid.total_rows()).filter_map(|index| grid.row(index)) {
            let cells = &line[..grid.cols().min(line.len())];
            let text: String = cells.iter().map(|cell| cell.ch()).collect();
            let drawn = cells.iter().filter(|cell| cell.ch() != ' ');
            let (hidden, total) = drawn.fold((0, 0), |(hidden, total), cell| {
                (hidden + usize::from(cell.is_hidden()), total + 1)
            });
            if total == 0 || text.starts_with("above") || text.starts_with("below") {
                assert_eq!(hidden, 0, "{text:?}");
            } else {
                assert_eq!(hidden, total, "{text:?}");
                hidden_rows += 1;
                hidden_text.push_str(&text);
            }
        }
        if let Some(banner_rows) = banner_rows {
            assert_eq!(hidden_rows, banner_rows);
        }
        let squeeze = |text: &str| -> String {
            text.chars()
                .filter(|&c| c != ' ' && c != DEV_CHANNEL_BANNER_BAR)
                .collect()
        };
        assert_eq!(
            squeeze(&hidden_text),
            squeeze(&DEV_CHANNEL_BANNER_LINES.concat())
        );
    }

    #[test]
    fn dev_banner_stays_hidden_while_scrolling_into_scrollback() {
        let mut terminal = Terminal::new_for_test(40, 12, 1);
        let (screen, rows) = banner_screen(DEV_CHANNEL_BANNER_LINES, 38);
        feed_dev_screen(&terminal, &screen);
        terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
        assert_only_banner_hidden(&terminal, Some(rows));
        // One row per batch: the block straddles the scrollback boundary,
        // its wrapped tail rows last on the screen, then leaves the screen.
        for _ in 0..rows + 12 {
            feed_dev_screen(&terminal, "\r\nbelow");
            terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
            assert_only_banner_hidden(&terminal, Some(rows));
        }
        let grid = crate::platform::lock_recover(&terminal.grid);
        assert!(screen_rows(&grid).flatten().all(|cell| !cell.is_hidden()));
    }

    #[test]
    fn dev_banner_pushed_into_scrollback_before_a_scan_is_hidden() {
        let mut terminal = Terminal::new_for_test(40, 12, 1);
        let (screen, rows) = banner_screen(DEV_CHANNEL_BANNER_LINES, 38);
        feed_dev_screen(&terminal, &format!("{screen}{}", "\r\nbelow".repeat(20)));
        terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
        assert_only_banner_hidden(&terminal, Some(rows));
    }

    #[test]
    fn dev_banner_soft_wrapped_by_the_terminal_or_reflowed_is_hidden() {
        // Notice rows wider than the terminal: autowrap continues each one
        // on rows without the bar.
        let mut terminal = Terminal::new_for_test(60, 12, 1);
        let (screen, _) = banner_screen(DEV_CHANNEL_BANNER_LINES, 190);
        feed_dev_screen(&terminal, &screen);
        terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
        assert_only_banner_hidden(&terminal, None);

        // Hidden on a wide screen, scrolled half into the scrollback, then
        // reflowed there and on the screen by resizes.
        let mut terminal = Terminal::new_for_test(200, 8, 1);
        feed_dev_screen(&terminal, &screen);
        terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
        assert_only_banner_hidden(&terminal, Some(2));
        feed_dev_screen(&terminal, &"\r\nbelow".repeat(6));
        terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
        assert_only_banner_hidden(&terminal, Some(2));
        {
            let grid = crate::platform::lock_recover(&terminal.grid);
            assert_eq!(grid.scrollback_len(), 2);
        }
        for cols in [60, 200, 30, 120, 30] {
            crate::platform::lock_recover(&terminal.grid).resize(cols, 8);
            terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
            assert_only_banner_hidden(&terminal, None);
        }
        let grid = crate::platform::lock_recover(&terminal.grid);
        let mut scrollback = scrollback_rows(&grid).flatten();
        assert!(scrollback.any(|cell| cell.is_hidden()));
    }

    /// Raw PTY output of `tg_cc` (Claude Code 2.1.284, fullscreen on the
    /// alternate screen) up to its notice: the header rows.
    const CLAUDE_HEADER: &str = "\x1b[?1049h\x1b[2J\x1b[H\x1b[?1000h\x1b[?1002h\x1b[?1003h\x1b[?1006h\x1b[?25l\x1b[H\r\x1b[1B\x1b[38;2;215;119;87m ▐\x1b[48;2;0;0;0m▛███▛█\x1b[12G\x1b[39m\x1b[49m\x1b[1mClaude Code\x1b[24G\x1b[22m\x1b[38;2;153;153;153mv2.1.284\r\x1b[1B\x1b[38;2;215;119;87m▝▜\x1b[48;2;0;0;0m█████\x1b[49m█▀\x1b[12G\x1b[38;2;153;153;153mOpus 5.5 (1M context) with medium effort · Claude Pro\r\x1b[1B\x1b[38;2;215;119;87m ▝▝   ▝▝ \x1b[12G\x1b[38;2;153;153;153m~/projects/car-wash-api\r\x1b[2B";
    /// The notice rows that follow `CLAUDE_HEADER` on a 120- and an 80-column
    /// PTY, joined by "\r\x1b[1B" in the output: Claude Code wraps the notice
    /// itself and draws the bar on every row.
    const CLAUDE_NOTICE: [(usize, [&str; 3]); 2] = [
        (120, [
            "\x1b[39m\x1b[2m▎\x1b[3G\x1b[22m\x1b[38;2;153;153;153mChannels (experimental) messages from server:cc-tg inject directly in this session · restart without ",
            "\x1b[39m\x1b[2m▎\x1b[3G\x1b[22m\x1b[38;2;153;153;153m--dangerously-load-development-channels to stop",
            "\x1b[39m\x1b[2m▎\x1b[3G\x1b[22m\x1b[38;2;255;193;7mserver:cc-tg · no MCP server configured with that name",
        ]),
        (80, [
            "\x1b[39m\x1b[2m▎\x1b[3G\x1b[22m\x1b[38;2;153;153;153mChannels (experimental) messages from server:cc-tg inject directly in this ",
            "\x1b[39m\x1b[2m▎\x1b[3G\x1b[22m\x1b[38;2;153;153;153msession · restart without --dangerously-load-development-channels to stop",
            "\x1b[39m\x1b[2m▎\x1b[3G\x1b[22m\x1b[38;2;255;193;7mserver:cc-tg · no MCP server configured with that name",
        ]),
    ];

    /// Asserts that exactly the screen rows in `hidden` have hidden cells,
    /// all of their non-blank cells.
    fn assert_hidden_screen_rows(terminal: &Terminal, hidden: std::ops::Range<usize>) {
        let rows = crate::platform::lock_recover(&terminal.grid).rows();
        for row in 0..rows {
            let (hidden_count, total) = hidden_cells(terminal, row);
            let expected = if hidden.contains(&row) { total } else { 0 };
            assert!(total > 0 || !hidden.contains(&row), "row {row}");
            assert_eq!(hidden_count, expected, "row {row}");
        }
    }

    #[test]
    fn dev_banner_scrolled_out_at_the_top_of_claude_fullscreen_is_hidden() {
        for (cols, notice) in CLAUDE_NOTICE {
            let mut terminal = Terminal::new_for_test(cols, 30, 1);
            let notice_rows = notice.join("\r\x1b[1B");
            feed_dev_screen(&terminal, &format!("{CLAUDE_HEADER}{notice_rows}\r\x1b[100C"));
            terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
            assert_hidden_screen_rows(&terminal, 5..8);
            // A wheel step: Claude Code redraws its transcript from the top
            // of the screen, the first notice rows scrolled out above it.
            // Its diff output skips unchanged cells; erasing each row first
            // gives the same screen.
            for cut in 1..notice.len() {
                let rows = &notice[cut..];
                feed_dev_screen(
                    &terminal,
                    &format!(
                        "\x1b[H\x1b[2K{}\r\x1b[1B\x1b[2K\r\x1b[1B\x1b[39m❯\u{a0}/clear\x1b[K\r\x1b[1B\x1b[J",
                        rows.join("\r\x1b[1B\x1b[2K")
                    ),
                );
                terminal.observe_dev_channel_banner(|_| Some(DEV_CLAUDE));
                assert_hidden_screen_rows(&terminal, 0..rows.len());
            }
            // The same rows below other output are not a cut block.
            feed_dev_screen(
                &terminal,
                &format!(
                    "\x1b[H\x1b[2Kshown\r\x1b[1B\x1b[2K{}\r\x1b[1B\x1b[J",
                    notice[1..].join("\r\x1b[1B\x1b[2K")
                ),
            );
            terminal.observe_dev_channel_banner(|_| panic!("no banner block is on the screen"));
            assert_hidden_screen_rows(&terminal, 0..0);
        }
    }

    #[test]
    fn screen_hash_is_sha256_of_exact_text() {
        let expected: [u8; 32] = Sha256::digest(b"screen").into();
        assert_eq!(screen_hash("screen"), expected);
    }

    #[test]
    fn direct_pty_command_preserves_argv_cwd_relative_program_and_env() {
        use std::ffi::OsString;
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "ronsole-direct-command-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = root.join("fixture");
        std::fs::write(
            &fixture,
            b"#!/bin/sh\nprintf '__DIRECT__\\r\\n'\nprintf '__ARG1__<%s>\\r\\n' \"$1\"\nprintf '__ARG2__<%s>\\r\\n' \"$2\"\nprintf '__ARG3__<%s>\\r\\n' \"$3\"\nprintf '__PWD__<%s>\\r\\n' \"$PWD\"\nprintf '__TERM__<%s>\\r\\n' \"$TERM\"\nprintf '__COLORTERM__<%s>\\r\\n' \"$COLORTERM\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();

        let launch = TerminalLaunchSpec {
            working_directory: Some(root.clone()),
            inherited_directory: None,
            command: vec![
                OsString::from("./fixture"),
                OsString::from("arg one"),
                OsString::from("--something"),
                OsString::from("русский/utf8"),
            ],
            hold: false,
            bridge_launch_id: None,
        };
        let mut terminal = Terminal::spawn(
            None,
            1,
            launch,
            crate::renderer::terminal_palette(crate::config::DEFAULT_TERMINAL_BACKGROUND),
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        let text = loop {
            let text = {
                let grid = crate::platform::lock_recover(&terminal.grid);
                (0..grid.total_rows())
                    .filter_map(|index| grid.row(index))
                    .flat_map(|line| line.iter().map(|cell| cell.ch()))
                    .collect::<String>()
            };
            if text.contains("__DIRECT__")
                && text.contains("__ARG1__<arg one>")
                && text.contains("__ARG2__<--something>")
                && text.contains("__ARG3__<русский/utf8>")
                && text.contains(&format!("__PWD__<{}>", root.display()))
                && text.contains("__TERM__<xterm-256color>")
                && text.contains("__COLORTERM__<truecolor>")
            {
                break text;
            }
            assert!(
                Instant::now() < deadline,
                "direct PTY command output did not reach the terminal grid: {text:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(text.contains("__DIRECT__"));

        let deadline = Instant::now() + Duration::from_secs(3);
        while !terminal.is_closed() {
            assert!(
                Instant::now() < deadline,
                "direct PTY command did not report exit"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        terminal.shutdown();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bug_70_poisoned_terminal_mutex_recovers_without_ui_thread_panic() {
        let grid = Mutex::new(test_term(4, 2));
        let mut guard = crate::platform::lock_recover(&grid);
        feed(&mut guard, b"R");
        assert_eq!(guard.visible_row(0).expect("screen row")[0].ch(), 'R');
        drop(guard);

        let source = include_str!("platform.rs");
        let production = source.split("#[cfg(test)]").next().unwrap_or(source);
        assert!(production.contains("unwrap_or_else(|poisoned| poisoned.into_inner())"));
        assert!(!production.contains("grid.lock().unwrap()"));
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TerminalPresentationIntent {
    #[default]
    None,
    ActivateWhenReady,
}

pub(crate) const BRIDGE_LAUNCH_TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Clone, Debug)]
pub(crate) struct BridgeLaunchState {
    pub(crate) bridge_launch_id: [u8; 16],
    pub(crate) created_at: Instant,
    pub(crate) enter_sent: bool,
}

impl BridgeLaunchState {
    pub(crate) fn new(bridge_launch_id: [u8; 16], created_at: Instant) -> Self {
        Self {
            bridge_launch_id,
            created_at,
            enter_sent: false,
        }
    }

    pub(crate) fn active_at(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.created_at) < BRIDGE_LAUNCH_TTL
    }

    fn should_auto_confirm(&self, now: Instant, screen: &str) -> bool {
        self.active_at(now) && !self.enter_sent && bridge_warning_visible(screen)
    }
}

/// Fragments of the Claude Code development-channels warning with whitespace
/// removed ("Loading development channels", the flag, and the selected
/// "❯ 1. I am using this for local development" choice).
const BRIDGE_WARNING_COMPACT_FRAGMENTS: [&str; 3] = [
    "Loadingdevelopmentchannels",
    "--dangerously-load-development-channels",
    "❯1.Iamusingthisforlocaldevelopment",
];

pub(crate) fn bridge_warning_visible(screen: &str) -> bool {
    let compact_screen: String = screen
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();
    compact_bridge_warning_visible(&compact_screen)
}

/// The warning matcher on screen text that already has no whitespace.
fn compact_bridge_warning_visible(compact_screen: &str) -> bool {
    BRIDGE_WARNING_COMPACT_FRAGMENTS
        .into_iter()
        .all(|fragment| compact_screen.contains(fragment))
}

/// Writes the current screen text without whitespace into `out`: the grid-side
/// equivalent of compacting `Terminal::screen_tail_text(usize::MAX)`, without
/// a per-call allocation once `out` has grown.
fn write_compact_screen(grid: &Term, out: &mut String) {
    out.clear();
    for line in (0..grid.rows()).filter_map(|y| grid.visible_row(y)) {
        for cell in line.iter().take(grid.cols()) {
            cell.append_text_to(out);
        }
    }
    out.retain(|character| !character.is_whitespace());
}

/// How long frames of a tab stay withheld after Enter was sent while the
/// development-channels warning is still on screen; after that the screen is
/// shown as is so the user can answer.
pub(crate) const DEV_WARNING_HIDE_LIMIT: Duration = Duration::from_millis(1500);

/// Per-tab state of the development-channels warning auto-confirmation.
#[derive(Debug, Default)]
struct DevWarningState {
    /// `Term::content_generation` of the last scanned screen.
    scanned_generation: Option<u64>,
    /// The last scanned screen matched the warning.
    visible: bool,
    /// When Enter was sent for the warning now on screen; cleared by the first
    /// scanned screen without the warning.
    enter_at: Option<Instant>,
    /// The `claude` process generation that got the automatic Enter.
    confirmed: Option<crate::tab_input::ProcGen>,
}

/// Text of the two Claude Code notice lines shown for the cc-tg development
/// channel, without the quote-border prefix (`DEV_CHANNEL_BANNER_BAR` and one
/// space) that Claude Code draws on every row of the notice, wrapped rows
/// included.
const DEV_CHANNEL_BANNER_LINES: [&str; 2] = [
    "Channels (experimental) messages from server:cc-tg inject directly in this session \u{b7} restart without --dangerously-load-development-channels to stop",
    "server:cc-tg \u{b7} no MCP server configured with that name",
];
const DEV_CHANNEL_BANNER_BAR: char = '\u{258e}';

/// Upper bound of the physical rows one banner block takes: every row holds
/// at least one character, so at least one byte, of the text.
const DEV_CHANNEL_BANNER_MAX_ROWS: usize =
    DEV_CHANNEL_BANNER_LINES[0].len() + DEV_CHANNEL_BANNER_LINES[1].len();

/// Matches one logical row (a row and its soft-wrapped continuations, as
/// `cells`) against `expected[pos..]`: optional leading blanks, the bar, one
/// space, then a non-empty segment equal to the next part of `expected`
/// (trailing blanks ignored). The space of `expected` at a wrap point may be
/// dropped by the wrapper. Returns the byte position after the segment.
fn match_banner_row<'a>(
    cells: impl Iterator<Item = &'a Cell>,
    expected: &str,
    pos: usize,
) -> Option<usize> {
    let mut cells = cells.filter(|cell| !cell.is_wide_spacer());
    let plain = |cell: &Cell, c: char| cell.ch() == c && cell.zero_width().is_empty();
    let bar = cells.find(|cell| cell.ch() != ' ')?;
    if !plain(bar, DEV_CHANNEL_BANNER_BAR) || !cells.next().is_some_and(|cell| plain(cell, ' ')) {
        return None;
    }
    let mut rest = expected.get(pos..)?;
    // Byte position in `expected` after the last matched non-blank cell.
    let mut end = None;
    while let Some(cell) = cells.next() {
        if end.is_none() && pos > 0 && cell.ch() != ' ' {
            rest = rest.strip_prefix(' ').unwrap_or(rest);
        }
        let mut chars = rest.chars();
        if !chars.next().is_some_and(|c| plain(cell, c)) {
            // Claude Code's side panel begins after the chat column. Its
            // divider and text share the terminal row with the banner.
            if cell.ch() != '│' {
                // Past the end of `expected` the text must end at a word
                // boundary: "names" is not the line "name".
                let continues = if rest.is_empty() {
                    cell.ch() != ' '
                } else {
                    cell.ch() != ' ' || !cells.all(|cell| cell.ch() == ' ')
                };
                if continues {
                    return None;
                }
            }
            if end.is_none() {
                return None;
            }
            break;
        }
        rest = chars.as_str();
        if cell.ch() != ' ' {
            end = Some(expected.len() - rest.len());
        }
    }
    end
}

/// Rows scanned for banner blocks: the last `history` scrollback rows (none
/// on the alternate screen), then the screen rows, as one sequence.
#[derive(Clone, Copy)]
struct BannerRows<'a> {
    grid: &'a Term,
    history: usize,
}

impl<'a> BannerRows<'a> {
    fn new(grid: &'a Term, history: usize) -> Self {
        let history = if grid.is_alt_screen() {
            0
        } else {
            history.min(grid.scrollback_len())
        };
        Self { grid, history }
    }

    fn len(self) -> usize {
        self.history + self.grid.rows()
    }

    /// Cells of row `index` (at most `cols`) and whether the row soft-wraps
    /// into the next one.
    fn row(self, index: usize) -> Option<(&'a [Cell], bool)> {
        let grid = self.grid;
        let absolute = match index.checked_sub(self.history) {
            Some(screen_row) => grid.scrollback_len() + screen_row,
            None => grid.scrollback_len() - self.history + index,
        };
        let line = grid.row(absolute)?;
        let cells = &line[..grid.cols().min(line.len())];
        Some((cells, grid.row_soft_wrapped(absolute)))
    }

    /// Number of rows of the logical row starting at `start`.
    fn logical_rows(self, start: usize) -> Option<usize> {
        let mut rows = 1;
        while self.row(start + rows - 1)?.1 && start + rows < self.len() {
            rows += 1;
        }
        Some(rows)
    }

    fn cells(self, start: usize, rows: usize) -> impl Iterator<Item = &'a Cell> {
        (start..start + rows).flat_map(move |index| self.row(index).map_or(&[][..], |row| row.0))
    }

    /// Number of rows taken by both `DEV_CHANNEL_BANNER_LINES`, one after the
    /// other, from the logical row starting at `start`, or `None` if the text
    /// differs.
    fn block_at(self, start: usize) -> Option<usize> {
        if start > 0 && self.row(start - 1)?.1 {
            return None;
        }
        self.rest_at(start, 0, 0)
    }

    /// Number of rows taken by the banner text from byte `pos` of
    /// `DEV_CHANNEL_BANNER_LINES[line]` to the end of the last line, from the
    /// logical row starting at `start`.
    fn rest_at(self, start: usize, line: usize, mut pos: usize) -> Option<usize> {
        let mut row = start;
        for expected in DEV_CHANNEL_BANNER_LINES.get(line..)? {
            while pos < expected.len() {
                let rows = self.logical_rows(row)?;
                pos = match_banner_row(self.cells(row, rows), expected, pos)?;
                row += rows;
            }
            pos = 0;
        }
        Some(row - start)
    }

    /// Number of rows of a block cut at the top: the first scanned row is the
    /// top of the content (alternate screen row 0, where Claude Code's
    /// fullscreen view scrolls its transcript out, or the oldest kept row),
    /// it starts a later part of the first banner line, or the second line,
    /// and the banner follows to its end. The whole second line is required.
    fn cut_block_at_top(self) -> Option<usize> {
        if !self.grid.is_alt_screen() && self.history < self.grid.scrollback_len() {
            return None;
        }
        let mut cells = self.row(0)?.0.iter().filter(|cell| !cell.is_wide_spacer());
        if cells.find(|cell| cell.ch() != ' ')?.ch() != DEV_CHANNEL_BANNER_BAR {
            return None;
        }
        let lead = cells.find(|cell| cell.ch() != ' ')?.ch();
        DEV_CHANNEL_BANNER_LINES[0]
            .char_indices()
            .skip(1)
            .filter(|&(_, c)| c == lead)
            .find_map(|(pos, _)| self.rest_at(0, 0, pos))
            .or_else(|| self.rest_at(0, 1, 0))
    }
}

/// Per-tab state of the development-channels banner hiding.
#[derive(Debug, Default)]
struct DevBannerState {
    /// `Term::content_generation` of the last applied scan.
    scanned_generation: Option<u64>,
    /// `Term::scrollback_pushed` and `Term::cols` at the last applied
    /// scan.
    scanned_pushed: u64,
    scanned_cols: usize,
    /// Some cell of this tab was hidden; until then no clearing pass runs.
    hid_any: bool,
    /// Foreground process group and whether it holds a flagged `claude`,
    /// kept while a banner block stays in the scanned rows.
    owner: Option<(Option<u32>, bool)>,
    /// Of the last `scan`: scrollback rows in the scanned rows, and the
    /// first scanned row that was not in the scrollback at the last applied
    /// scan.
    window: (usize, usize),
    /// Of the last `scan`: banner blocks as (first scanned row, rows).
    blocks: Vec<(usize, usize)>,
}

impl DevBannerState {
    /// Finds the banner blocks on the screen and in the scrollback rows that
    /// entered it since the last applied scan, plus `DEV_CHANNEL_BANNER_MAX_ROWS`
    /// rows above them, so a block that scrolled partly or wholly out of the
    /// screen is still matched from its first row; a block whose first rows
    /// are above the top of the content is matched from its first kept row
    /// (`BannerRows::cut_block_at_top`). After a width change the whole
    /// scrollback is scanned, since the resize reflowed it. Returns whether
    /// there is a block and whether one has a drawn (not hidden) cell.
    fn scan(&mut self, grid: &Term) -> (bool, bool) {
        let pushed = usize::try_from(grid.scrollback_pushed().wrapping_sub(self.scanned_pushed))
            .unwrap_or(usize::MAX);
        let reach = if grid.cols() == self.scanned_cols {
            pushed.saturating_add(DEV_CHANNEL_BANNER_MAX_ROWS)
        } else {
            usize::MAX
        };
        let rows = BannerRows::new(grid, reach);
        self.window = (rows.history, rows.history.saturating_sub(pushed));
        self.blocks.clear();
        let mut drawn = false;
        let mut row = 0;
        while row < rows.len() {
            let block = rows.block_at(row);
            let block = if row == 0 {
                block.or_else(|| rows.cut_block_at_top())
            } else {
                block
            };
            let Some(len) = block else {
                row += 1;
                continue;
            };
            drawn |= rows
                .cells(row, len)
                .any(|cell| cell.ch() != ' ' && !cell.is_hidden());
            self.blocks.push((row, len));
            row += len;
        }
        (!self.blocks.is_empty(), drawn)
    }

    /// Hides the non-blank cells of every block of the last `scan` when
    /// `hide_new` (already hidden blocks stay hidden either way) and clears
    /// the hidden flag on every other screen row and on every other row that
    /// entered the scrollback since the last applied scan.
    fn apply(&self, grid: &mut Term, hide_new: bool) {
        let (history, fresh) = self.window;
        let first = grid.scrollback_len().saturating_sub(history);
        let cols = grid.cols();
        let mut blocks = self.blocks.iter().copied().peekable();
        for index in 0..history + grid.rows() {
            // Blocks are disjoint and `index` steps by one: at most one ends.
            let _ = blocks.next_if(|&(start, len)| start + len <= index);
            let in_block = blocks.peek().is_some_and(|&(start, _)| start <= index);
            if (in_block && !hide_new) || (!in_block && index < fresh) {
                continue;
            }
            // `index` is relative to the scanned rows; `row()` indexes the
            // whole sequence, so the scrollback offset is added back here.
            let absolute = first + index;
            let Some(line) = grid.row(absolute) else {
                continue;
            };
            if !in_block {
                grid.set_host_hidden(absolute, 0..cols, false);
                continue;
            }
            // Only drawn cells before the banner's separator bar are hidden.
            let end = line.len().min(cols);
            let stop = line[..end]
                .iter()
                .position(|cell| cell.ch() == '│')
                .unwrap_or(end);
            // `line` ends its borrow here, so each cell is re-read through a
            // short borrow before the mutable `set_host_hidden` call.
            for x in 0..stop {
                let drawn = grid
                    .row(absolute)
                    .is_some_and(|row_cells| row_cells[x].ch() != ' ');
                if drawn {
                    grid.set_host_hidden(absolute, x..x + 1, true);
                }
            }
        }
    }

    fn mark_scanned(&mut self, grid: &Term) {
        self.scanned_generation = Some(grid.content_generation());
        self.scanned_pushed = grid.scrollback_pushed();
        self.scanned_cols = grid.cols();
    }
}

pub(crate) fn screen_hash(screen: &str) -> [u8; 32] {
    Sha256::digest(screen.as_bytes()).into()
}

pub struct Terminal {
    pub grid: Arc<Mutex<Term>>,
    bridge_screen_lock: Arc<Mutex<()>>,
    process: Option<crate::terminal_process::TerminalProcess>,
    hold: bool,
    pub scroll_y: crate::scroll::ScrollState,
    pub(crate) presentation_intent: TerminalPresentationIntent,
    pub(crate) reveal_right_tail_when_presented: bool,
    title_cache: crate::terminal_process::TerminalTitleCache,
    tab_token: crate::terminal_process::TabToken,
    last_user_input: Option<Instant>,
    user_input_held: bool,
    held_user_input: Vec<u8>,
    bridge_launch: Option<BridgeLaunchState>,
    dev_warning: DevWarningState,
    dev_banner: DevBannerState,
    /// Working directory from the latest OSC 7 report that passed
    /// `accept_osc7_cwd`; a new tab inherits it.
    pub(crate) osc7_cwd: Option<PathBuf>,
    /// Sender clone kept for `flush_expired`; the parser thread holds another.
    reply_tx: SyncSender<Vec<u8>>,
    pending: Arc<Mutex<PendingHostEvents>>,
    wake: Option<WakeHandle>,
    #[cfg(test)]
    test_input: Option<Arc<Mutex<Vec<u8>>>>,
}

/// Upper bound for user input buffered by `Terminal::begin_input_hold`.
const TERMINAL_HELD_INPUT_MAX_BYTES: usize = 256 * 1024;

fn write_terminal_spawn_error(term: &mut Term, error: &io::Error) {
    let message = format!("Ronsole terminal error: {error}\r\n");
    let mut sink = SessionSink {
        reply_tx: None,
        title_cache: None,
        pending: None,
        wake: None,
    };
    term.advance(message.as_bytes(), Instant::now(), &mut sink);
    term.mark_presentation_ready();
}

/// Longest OSC 7 path, in bytes after percent-decoding, that a tab may adopt.
const OSC7_PATH_MAX_BYTES: usize = 4096;

/// Accepts an OSC 7 report only when it names this machine (empty host,
/// `localhost`, or `local_host`) and carries a usable absolute UTF-8 path
/// without NUL bytes. Anything else returns `None`, and the previous
/// directory stays in place.
fn accept_osc7_cwd(host: &str, path: &[u8], local_host: &str) -> Option<PathBuf> {
    if !(host.is_empty() || host == "localhost" || host == local_host) {
        return None;
    }
    if path.is_empty() || path.len() > OSC7_PATH_MAX_BYTES || path.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(path).ok()?;
    let path = Path::new(text);
    path.is_absolute().then(|| path.to_path_buf())
}

/// This machine's host name, read once per process on first use.
fn local_host_name() -> &'static str {
    static LOCAL_HOST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LOCAL_HOST.get_or_init(read_host_name)
}

fn read_host_name() -> String {
    let mut buffer = [0_u8; 256];
    // SAFETY: `buffer` is valid for writes of its full length.
    let status = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if status != 0 {
        return String::new();
    }
    let len = buffer
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..len]).into_owned()
}

/// Earliest synchronized-output deadline across all tabs; `None` when no tab
/// holds bytes.
pub(crate) fn earliest_terminal_deadline(terminals: &[Terminal]) -> Option<Instant> {
    terminals.iter().filter_map(Terminal::sync_deadline).min()
}

impl Terminal {
    #[cfg(test)]
    pub(crate) fn new_for_test(cols: usize, rows: usize, display_number: u64) -> Self {
        let title_cache = Arc::new(Mutex::new(
            crate::terminal_process::TerminalTitleState::new_numbered(
                "terminal".to_string(),
                display_number,
            ),
        ));
        Self {
            grid: Arc::new(Mutex::new(new_term(
                cols,
                rows,
                crate::renderer::terminal_palette(crate::config::DEFAULT_TERMINAL_BACKGROUND),
            ))),
            bridge_screen_lock: Arc::new(Mutex::new(())),
            process: None,
            hold: false,
            scroll_y: crate::scroll::ScrollState::new(7.0),
            presentation_intent: TerminalPresentationIntent::None,
            reveal_right_tail_when_presented: false,
            title_cache,
            tab_token: crate::terminal_process::TabToken::generate(),
            last_user_input: None,
            user_input_held: false,
            held_user_input: Vec::new(),
            bridge_launch: None,
            dev_warning: DevWarningState::default(),
            dev_banner: DevBannerState::default(),
            osc7_cwd: None,
            // No reader exists for test tabs, so replies are dropped unless a
            // test installs its own sender.
            reply_tx: terminal_reply_channel().0,
            pending: Arc::new(Mutex::new(PendingHostEvents::default())),
            wake: None,
            test_input: Some(Arc::new(Mutex::new(Vec::new()))),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_bridge_for_test(
        cols: usize,
        rows: usize,
        display_number: u64,
        bridge_launch_id: [u8; 16],
    ) -> Self {
        let mut terminal = Self::new_for_test(cols, rows, display_number);
        terminal.bridge_launch = Some(BridgeLaunchState::new(bridge_launch_id, Instant::now()));
        terminal
    }

    pub(crate) fn spawn(
        wake: Option<crate::wake::WakeHandle>,
        display_number: u64,
        launch: TerminalLaunchSpec,
        palette: TermPalette,
    ) -> Self {
        let hold = launch.hold;
        let bridge_launch = launch
            .bridge_launch_id
            .map(|bridge_launch_id| BridgeLaunchState::new(bridge_launch_id, Instant::now()));
        let title_cache = Arc::new(Mutex::new(
            crate::terminal_process::TerminalTitleState::new_numbered(
                "terminal".to_string(),
                display_number,
            ),
        ));
        let grid = Arc::new(Mutex::new(new_term(200, 60, palette)));
        let bridge_screen_lock = Arc::new(Mutex::new(()));
        let tab_token = crate::terminal_process::TabToken::generate();
        let (reply_tx, reply_rx) = terminal_reply_channel();
        let pending = Arc::new(Mutex::new(PendingHostEvents::default()));
        let process = match crate::terminal_process::TerminalProcess::spawn(
            grid.clone(),
            bridge_screen_lock.clone(),
            title_cache.clone(),
            wake.clone(),
            HostChannels {
                reply_tx: reply_tx.clone(),
                reply_rx,
                pending: pending.clone(),
            },
            launch,
            &tab_token,
        ) {
            Ok(process) => Some(process),
            Err(error) => {
                crate::platform::lock_recover(&title_cache)
                    .set_fallback("terminal error".to_string());
                let mut grid = crate::platform::lock_recover(&grid);
                write_terminal_spawn_error(&mut grid, &error);
                None
            }
        };

        Self {
            grid,
            bridge_screen_lock,
            process,
            hold,
            scroll_y: crate::scroll::ScrollState::new(7.0),
            presentation_intent: TerminalPresentationIntent::None,
            reveal_right_tail_when_presented: false,
            title_cache,
            tab_token,
            last_user_input: None,
            user_input_held: false,
            held_user_input: Vec::new(),
            bridge_launch,
            dev_warning: DevWarningState::default(),
            dev_banner: DevBannerState::default(),
            osc7_cwd: None,
            reply_tx,
            pending,
            wake,
            #[cfg(test)]
            test_input: None,
        }
    }

    /// Earliest synchronized-output deadline of this tab, if it holds bytes.
    /// Takes the grid lock briefly.
    pub(crate) fn sync_deadline(&self) -> Option<Instant> {
        crate::platform::lock_recover(&self.grid).sync_deadline()
    }

    /// Replays held synchronized-output bytes whose deadline has passed at
    /// `now`, using the same sink as the parser thread. Marks the grid changed
    /// and wakes the main loop when the replay changed it.
    pub(crate) fn flush_expired(&self, now: Instant) {
        let outcome = {
            // Same lock order as the parser thread: screen lock, then grid.
            let _screen_guard = crate::platform::lock_recover(&self.bridge_screen_lock);
            let mut grid = crate::platform::lock_recover(&self.grid);
            let mut sink = SessionSink {
                reply_tx: Some(&self.reply_tx),
                title_cache: Some(&self.title_cache),
                pending: Some(&self.pending),
                wake: self.wake.as_ref(),
            };
            let outcome = grid.flush_expired(now, &mut sink);
            if outcome.grid_changed {
                grid.mark_changed();
            }
            outcome
        };
        notify_main_loop(self.wake.as_ref(), outcome);
    }

    /// Applies the clipboard and working-directory events parked by the
    /// parser thread. Called on the main thread; holds only the pending lock.
    pub(crate) fn apply_host_events(&mut self, interaction: &mut TerminalInteraction) {
        let events = PendingHostEvents::take(&self.pending);
        if let Some(text) = events.clipboard {
            let _ = interaction.set_clipboard_text(text);
        }
        if let Some((host, path)) = events.cwd
            && let Some(cwd) = accept_osc7_cwd(&host, &path, local_host_name())
        {
            self.osc7_cwd = Some(cwd);
        }
    }

    /// Replaces the reply sender used by [`Terminal::flush_expired`]; tests
    /// use it to observe replies without a PTY.
    #[cfg(test)]
    pub(crate) fn set_reply_sender_for_test(&mut self, reply_tx: SyncSender<Vec<u8>>) {
        self.reply_tx = reply_tx;
    }

    pub(crate) fn bridge_launch_state(&self) -> Option<&BridgeLaunchState> {
        self.bridge_launch.as_ref()
    }

    pub(crate) fn bridge_launch_state_mut(&mut self) -> Option<&mut BridgeLaunchState> {
        self.bridge_launch.as_mut()
    }

    pub(crate) fn clear_bridge_launch_state(&mut self) {
        self.bridge_launch = None;
    }

    #[cfg(test)]
    pub(crate) fn input_bytes_for_test(&self) -> Vec<u8> {
        self.test_input
            .as_ref()
            .map(|input| crate::platform::lock_recover(input).clone())
            .unwrap_or_default()
    }

    pub(crate) fn maybe_auto_confirm_bridge(&mut self, now: Instant) -> bool {
        if !self
            .bridge_launch
            .as_ref()
            .is_some_and(|state| state.active_at(now) && !state.enter_sent)
        {
            return false;
        }
        let screen = self.screen_tail_text(usize::MAX).join("\n");
        let should_send = self
            .bridge_launch
            .as_ref()
            .is_some_and(|state| state.should_auto_confirm(now, &screen));
        if !should_send || self.write_input(b"\r").is_err() {
            return false;
        }
        if let Some(state) = self.bridge_launch.as_mut() {
            state.enter_sent = true;
        }
        // The warning was matched just now: the generic path must not send a
        // second Enter for it.
        self.dev_warning.enter_at = Some(now);
        true
    }

    /// Scans the screen for the development-channels warning when its content
    /// changed. On a matching screen with no Enter sent for it yet, writes one
    /// Enter if `find_claude` (given the PTY foreground process group) finds a
    /// `claude --dangerously-load-development-channels` generation that has
    /// not been confirmed in this tab before. `scratch` is a reusable buffer.
    /// Returns whether Enter was written.
    pub(crate) fn observe_dev_channel_warning(
        &mut self,
        now: Instant,
        scratch: &mut String,
        find_claude: impl FnOnce(Option<u32>) -> Option<crate::tab_input::ProcGen>,
    ) -> bool {
        {
            let grid = crate::platform::lock_recover(&self.grid);
            if self.dev_warning.scanned_generation == Some(grid.content_generation()) {
                return false;
            }
            self.dev_warning.scanned_generation = Some(grid.content_generation());
            write_compact_screen(&grid, scratch);
        }
        self.dev_warning.visible = compact_bridge_warning_visible(scratch);
        if !self.dev_warning.visible {
            self.dev_warning.enter_at = None;
            return false;
        }
        if self.dev_warning.enter_at.is_some() {
            return false;
        }
        let Some(claude) = find_claude(self.process_group_leader()) else {
            return false;
        };
        if self.dev_warning.confirmed == Some(claude) || self.write_input(b"\r").is_err() {
            return false;
        }
        self.dev_warning.confirmed = Some(claude);
        self.dev_warning.enter_at = Some(now);
        if let Some(state) = self.bridge_launch.as_mut() {
            state.enter_sent = true;
        }
        true
    }

    /// End of the frame hiding for this tab: `Some` while the last scanned
    /// screen shows the warning that Enter was already sent for. Frames are
    /// withheld until this instant (`DEV_WARNING_HIDE_LIMIT` after Enter).
    pub(crate) fn dev_warning_hidden_until(&self) -> Option<Instant> {
        if !self.dev_warning.visible {
            return None;
        }
        self.dev_warning
            .enter_at
            .map(|at| at + DEV_WARNING_HIDE_LIMIT)
    }

    pub(crate) fn dev_warning_frame_hidden(&self, now: Instant) -> bool {
        self.dev_warning_hidden_until()
            .is_some_and(|until| now < until)
    }

    /// When the grid content changed, hides the cells of the exact
    /// `DEV_CHANNEL_BANNER_LINES` rows (on the screen and in the scrollback,
    /// see `DevBannerState::scan`) if `find_claude` (given the PTY foreground
    /// process group) finds a flagged `claude`; the answer is cached per group
    /// while a banner stays in the scanned rows, and it is asked only for a
    /// banner with drawn cells. Only the cell hidden flag changes: text,
    /// cursor, scrollback and selection stay as the program wrote them.
    pub(crate) fn observe_dev_channel_banner(
        &mut self,
        find_claude: impl FnOnce(Option<u32>) -> Option<crate::tab_input::ProcGen>,
    ) {
        let generation = {
            let mut grid = crate::platform::lock_recover(&self.grid);
            let generation = grid.content_generation();
            if self.dev_banner.scanned_generation == Some(generation) {
                return;
            }
            let (any, drawn) = self.dev_banner.scan(&grid);
            if !any {
                self.dev_banner.owner = None;
            }
            if !drawn {
                if self.dev_banner.hid_any {
                    self.dev_banner.apply(&mut grid, false);
                }
                self.dev_banner.mark_scanned(&grid);
                return;
            }
            generation
        };
        // The process lookup runs without the grid lock.
        let pgrp = self.process_group_leader();
        let flagged = match self.dev_banner.owner {
            Some((cached, flagged)) if cached == pgrp => flagged,
            _ => {
                let flagged = find_claude(pgrp).is_some();
                self.dev_banner.owner = Some((pgrp, flagged));
                flagged
            }
        };
        let mut grid = crate::platform::lock_recover(&self.grid);
        // New output arrived meanwhile: the next call scans it.
        if grid.content_generation() != generation {
            return;
        }
        if flagged || self.dev_banner.hid_any {
            self.dev_banner.scan(&grid);
            self.dev_banner.apply(&mut grid, flagged);
            self.dev_banner.hid_any |= flagged;
        }
        self.dev_banner.mark_scanned(&grid);
    }

    pub(crate) fn manually_confirm_bridge(
        &mut self,
        now: Instant,
        expected_hash: [u8; 32],
    ) -> bool {
        let _screen_guard = crate::platform::lock_recover(&self.bridge_screen_lock);
        let screen = self.screen_tail_text(usize::MAX).join("\n");
        if expected_hash != screen_hash(&screen) {
            return false;
        }
        if !self
            .bridge_launch
            .as_ref()
            .is_some_and(|state| state.active_at(now) && !state.enter_sent)
        {
            return false;
        }
        if self.write_input(b"\r").is_err() {
            return false;
        }
        if let Some(state) = self.bridge_launch.as_mut() {
            state.enter_sent = true;
        }
        if bridge_warning_visible(&screen) {
            self.dev_warning.enter_at = Some(now);
        }
        true
    }

    pub(crate) fn tab_token(&self) -> &str {
        self.tab_token.as_str()
    }

    /// Writes keyboard/text/IME/paste input. Marks the time of user input and,
    /// while `begin_input_hold` is active, buffers the bytes in arrival order.
    pub(crate) fn write_user_input(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if bytes.iter().any(|byte| matches!(byte, b'\r' | b'\n'))
            && let Some(state) = self.bridge_launch_state_mut()
        {
            state.enter_sent = true;
        }
        self.last_user_input = Some(Instant::now());
        if !self.user_input_held {
            let _ = self.write_input(bytes);
            return;
        }
        if self.held_user_input.len().saturating_add(bytes.len()) > TERMINAL_HELD_INPUT_MAX_BYTES {
            // Over the bound: release everything now rather than drop or reorder bytes.
            let held = std::mem::take(&mut self.held_user_input);
            let _ = self.write_input(&held);
            let _ = self.write_input(bytes);
            return;
        }
        self.held_user_input.extend_from_slice(bytes);
    }

    pub(crate) fn last_user_input(&self) -> Option<Instant> {
        self.last_user_input
    }

    #[cfg(test)]
    pub(crate) fn set_last_user_input_for_test(&mut self, at: Option<Instant>) {
        self.last_user_input = at;
    }

    #[cfg(test)]
    pub(crate) fn input_hold_active(&self) -> bool {
        self.user_input_held
    }

    #[cfg(test)]
    pub(crate) fn break_input_writer_for_test(&self) {
        if let Some(process) = self.process.as_ref() {
            process.break_writer_for_test();
        }
    }

    pub(crate) fn begin_input_hold(&mut self) {
        self.user_input_held = true;
    }

    /// Ends the hold and writes the buffered user input with one `write_input`.
    pub(crate) fn end_input_hold(&mut self) {
        self.user_input_held = false;
        if self.held_user_input.is_empty() {
            return;
        }
        let held = std::mem::take(&mut self.held_user_input);
        let _ = self.write_input(&held);
    }

    pub(crate) fn process_group_leader(&self) -> Option<u32> {
        self.process.as_ref()?.process_group_leader()
    }

    /// Text of the last `rows` lines of the current screen (alt grid while in
    /// alt-screen), trailing spaces trimmed.
    pub(crate) fn screen_tail_text(&self, rows: usize) -> Vec<String> {
        let grid = crate::platform::lock_recover(&self.grid);
        let start = grid.rows().saturating_sub(rows);
        (start..grid.rows())
            .filter_map(|y| grid.visible_row(y))
            .map(|line| {
                let mut text = String::with_capacity(line.len());
                for cell in line.iter().take(grid.cols()) {
                    cell.append_text_to(&mut text);
                }
                text.truncate(text.trim_end_matches(' ').len());
                text
            })
            .collect()
    }

    pub fn write_display_title(&self, output: &mut String) {
        crate::platform::lock_recover(&self.title_cache).write_resolved(output);
    }

    pub(crate) fn presentation_ready(&self) -> bool {
        crate::platform::lock_recover(&self.grid).presentation_ready()
    }

    pub fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if let Some(input) = self.test_input.as_ref() {
            crate::platform::lock_recover(input).extend_from_slice(bytes);
            return Ok(());
        }
        self.process
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "terminal is not running"))?
            .write_input(bytes)
    }

    pub fn is_closed(&mut self) -> bool {
        !self.hold
            && self
                .process
                .as_mut()
                .is_some_and(|process| process.try_wait().unwrap_or(true))
    }

    pub(crate) fn take_finished_held_process_for_cleanup(
        &mut self,
    ) -> Option<crate::terminal_process::TerminalProcess> {
        if !self.hold {
            return None;
        }
        let finished = self
            .process
            .as_mut()
            .is_some_and(|process| process.try_wait().unwrap_or(true));
        if finished { self.process.take() } else { None }
    }

    #[cfg(test)]
    pub(crate) fn has_process(&self) -> bool {
        self.process.is_some()
    }

    pub fn shutdown(&mut self) {
        if let Some(process) = self.process.as_mut() {
            process.shutdown();
        }
        self.process = None;
    }

    pub(crate) fn take_process_for_cleanup(
        &mut self,
    ) -> Option<crate::terminal_process::TerminalProcess> {
        self.process.take()
    }

    pub(crate) fn restore_process_after_cleanup_backpressure(
        &mut self,
        process: crate::terminal_process::TerminalProcess,
    ) {
        debug_assert!(self.process.is_none());
        self.process = Some(process);
    }

    pub fn resize_pty(&self, cols: u16, rows: u16) {
        if let Some(process) = self.process.as_ref() {
            let _ = process.resize(cols, rows);
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}
