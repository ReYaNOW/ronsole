use super::support::{RecordingSink, feed, set_line, test_config};
use crate::cell::{
    CELL_PRESENTATION_AUTO, CELL_PRESENTATION_EMOJI, CELL_PRESENTATION_TEXT, Cell,
    is_terminal_zero_width_format, terminal_presentation_selector,
};
use crate::color::TerminalColor;
use crate::term::Term;

#[test]
fn terminal_print_applies_text_selector_but_keeps_text_default_symbols_text() {
    let mut grid = Term::new(test_config(8, 2));
    let mut sink = RecordingSink::new();

    feed(&mut grid, &mut sink, "✔\u{FE0F}X✅\u{FE0E}Y".as_bytes());

    let mut reconstructed = String::new();
    for cell in &grid.lines[0][0..5] {
        cell.append_text_to(&mut reconstructed);
    }
    assert_eq!(reconstructed, "✔\u{FE0F}X✅\u{FE0E}Y");
    assert_eq!(grid.cur_x, 5);
    assert_eq!(grid.lines[0][0].presentation(), CELL_PRESENTATION_TEXT);
    assert_eq!(grid.lines[0][1].presentation(), CELL_PRESENTATION_AUTO);
    assert_eq!(grid.lines[0][2].presentation(), CELL_PRESENTATION_TEXT);
    assert!(grid.lines[0][2].is_wide());
    assert!(grid.lines[0][3].is_wide_spacer());
    assert_eq!(grid.lines[0][4].presentation(), CELL_PRESENTATION_AUTO);
    assert_eq!(
        terminal_presentation_selector('\u{FE0F}'),
        Some(CELL_PRESENTATION_EMOJI)
    );
    assert_eq!(
        terminal_presentation_selector('\u{FE0E}'),
        Some(CELL_PRESENTATION_TEXT)
    );
    assert!(is_terminal_zero_width_format('\u{FE0F}'));
    assert!(is_terminal_zero_width_format('\u{FE0E}'));
    assert!(is_terminal_zero_width_format('\u{200D}'));
    assert!(!is_terminal_zero_width_format('✔'));
}

#[test]
fn terminal_cell_presentation_flag_keeps_cell_size_tight() {
    assert!(std::mem::size_of::<Cell>() <= 24);
    assert!(Cell::default().zero_width().is_empty());
}

#[test]
fn terminal_csi_cursor_modes_colors_and_replies_end_to_end() {
    let mut grid = Term::new(test_config(8, 3));
    let mut sink = RecordingSink::new();

    feed(
        &mut grid,
        &mut sink,
        b"abc\x1b[2D!\x1b[s\x1b[3;8H?\x1b[u\x1b[31;44;1mX\x1b[22mY\x1b[38;5;200;48;5;17mZ",
    );

    assert_eq!(grid.lines[0][0].ch(), 'a');
    assert_eq!(grid.lines[0][1].ch(), '!');
    assert_eq!(grid.lines[2][7].ch(), '?');
    assert_eq!(
        (grid.lines[0][2].ch(), grid.lines[0][2].fg(), grid.lines[0][2].bg()),
        ('X', TerminalColor::indexed(9), TerminalColor::indexed(4))
    );
    assert_eq!(
        (grid.lines[0][3].ch(), grid.lines[0][3].fg(), grid.lines[0][3].bg()),
        ('Y', TerminalColor::indexed(1), TerminalColor::indexed(4))
    );
    assert_eq!(
        (grid.lines[0][4].ch(), grid.lines[0][4].fg(), grid.lines[0][4].bg()),
        ('Z', TerminalColor::indexed(200), TerminalColor::indexed(17))
    );

    feed(&mut grid, &mut sink, b"\x1b[?25l\x1b[?1h\x1b[?1000h");
    assert!(!grid.cursor_visible);
    assert!(grid.app_cursor_keys);
    assert!(grid.mouse_tracking_mode.enabled());

    feed(&mut grid, &mut sink, b"\x1b[6n\x1b[c\x1b]10;?\x1b\\");
    // The batch yields one `reply` holding the replies in the order the queries were fed.
    assert_eq!(sink.calls.len(), 1);
    let replies = String::from_utf8(sink.bytes.clone()).unwrap();
    let (reply_pos, rest) = replies.split_once("\x1B[?62c").unwrap();
    assert!(reply_pos.starts_with("\x1B[1;6R"));
    assert_eq!(rest, "\x1B]10;rgb:ffff/ffff/ffff\x1B\\");

    feed(&mut grid, &mut sink, b"\x1b[?1049hALT\x1b[?1049l");
    assert!(!grid.is_alt);
    assert!(grid.alt_lines.is_none());
    assert_eq!(grid.lines[0][0].ch(), 'a');
}

#[test]
fn terminal_csi_erases_scroll_region_and_line_mutation_end_to_end() {
    let mut grid = Term::new(test_config(5, 4));
    let mut sink = RecordingSink::new();
    set_line(&mut grid, 0, "abcde");
    set_line(&mut grid, 1, "fghij");
    set_line(&mut grid, 2, "klmno");
    set_line(&mut grid, 3, "pqrst");

    feed(&mut grid, &mut sink, b"\x1b[2;2H\x1b[K");
    assert_eq!(grid.lines[1][0].ch(), 'f');
    assert_eq!(grid.lines[1][1].ch(), ' ');

    feed(&mut grid, &mut sink, b"\x1b[1;3H\x1b[1K");
    assert_eq!(grid.lines[0][0].ch(), ' ');
    assert_eq!(grid.lines[0][3].ch(), 'd');

    feed(&mut grid, &mut sink, b"\x1b[2J");
    assert!(
        grid.lines
            .iter()
            .flat_map(|line| line.iter())
            .all(|cell| cell.ch() == ' ')
    );

    set_line(&mut grid, 0, "11111");
    set_line(&mut grid, 1, "22222");
    set_line(&mut grid, 2, "33333");
    set_line(&mut grid, 3, "44444");
    feed(&mut grid, &mut sink, b"\x1b[2;3r\x1b[2;1H\x1b[LAAAAA\x1b[2;1H\x1b[M");
    assert_eq!(grid.scroll_region, (1, 2));
    assert_eq!(grid.cur_y, 1);

    feed(&mut grid, &mut sink, b"\x1b[1;1H12345\x1b[1;3H\x1b[2P");
    let top: String = grid.lines[0].iter().map(|cell| cell.ch()).collect();
    assert_eq!(top, "125  ");

    feed(&mut grid, &mut sink, b"\x1b[1;1Habcde\x1b[1;2H\x1b[3X");
    let top: String = grid.lines[0].iter().map(|cell| cell.ch()).collect();
    assert_eq!(top, "a   e");

    feed(&mut grid, &mut sink, b"\x1b[3J");
    assert!(grid.scrollback.is_empty());
}

#[test]
fn csi_saved_erase_clears_history_without_touching_viewport_or_cursor() {
    let mut grid = Term::new(test_config(7, 2));
    let mut sink = RecordingSink::new();
    let mut old = vec![Cell::default(); 7];
    for (index, c) in "old    ".chars().enumerate() {
        old[index].set_ch(c);
    }
    grid.push_scrollback_row_for_test(old, 7, false);
    set_line(&mut grid, 0, "visible");
    set_line(&mut grid, 1, "screen ");
    grid.cur_x = 3;
    grid.cur_y = 1;
    grid.selection = Some((0, 0, 2, 0));
    let before_viewport = grid.lines.clone();
    let before_cursor = (grid.cur_x, grid.cur_y);

    feed(&mut grid, &mut sink, b"\x1b[3J");

    assert!(grid.scrollback.is_empty());
    assert_eq!(grid.lines, before_viewport);
    assert_eq!((grid.cur_x, grid.cur_y), before_cursor);
    assert!(grid.selection.is_none());

    feed(&mut grid, &mut sink, b"\x1b[2J");
    assert!(
        grid.lines
            .iter()
            .flat_map(|line| line.iter())
            .all(|cell| cell.ch() == ' ')
    );
}

#[test]
fn insert_delete_lines_are_noop_outside_scroll_region() {
    let mut grid = Term::new(test_config(5, 5));
    let mut sink = RecordingSink::new();
    for (row, text) in ["11111", "22222", "33333", "44444", "55555"]
        .into_iter()
        .enumerate()
    {
        set_line(&mut grid, row, text);
    }
    feed(&mut grid, &mut sink, b"\x1b[2;4r\x1b[1;1H");
    let before = grid.lines.clone();
    feed(&mut grid, &mut sink, b"\x1b[L");
    assert_eq!(grid.lines, before);
    feed(&mut grid, &mut sink, b"\x1b[M");
    assert_eq!(grid.lines, before);

    feed(&mut grid, &mut sink, b"\x1b[2;1H\x1b[L");
    assert_eq!(
        grid.lines[0].iter().map(|cell| cell.ch()).collect::<String>(),
        "11111"
    );
    assert_eq!(
        grid.lines[1].iter().map(|cell| cell.ch()).collect::<String>(),
        "     "
    );
    assert_eq!(
        grid.lines[2].iter().map(|cell| cell.ch()).collect::<String>(),
        "22222"
    );
    assert_eq!(
        grid.lines[4].iter().map(|cell| cell.ch()).collect::<String>(),
        "55555"
    );

    feed(&mut grid, &mut sink, b"\x1b[2;1H\x1b[M");
    assert_eq!(
        grid.lines[0].iter().map(|cell| cell.ch()).collect::<String>(),
        "11111"
    );
    assert_eq!(
        grid.lines[4].iter().map(|cell| cell.ch()).collect::<String>(),
        "55555"
    );
}

#[test]
fn terminal_resize_preserves_scrollback_saved_cursor_and_alt_buffer() {
    let mut grid = Term::new(test_config(4, 4));
    let mut sink = RecordingSink::new();
    set_line(&mut grid, 0, "aaaa");
    set_line(&mut grid, 1, "bbbb");
    set_line(&mut grid, 2, "cccc");
    set_line(&mut grid, 3, "dddd");
    grid.cur_y = 3;
    grid.saved_cursor = Some((1, 3));

    grid.resize(4, 2);

    assert_eq!(grid.scrollback.len(), 2);
    assert_eq!(grid.cur_y, 1);
    assert_eq!(grid.saved_cursor, Some((1, 1)));
    let row0: String = grid.lines[0].iter().map(|cell| cell.ch()).collect();
    let row1: String = grid.lines[1].iter().map(|cell| cell.ch()).collect();
    assert_eq!(row0, "cccc");
    assert_eq!(row1, "dddd");

    grid.resize(4, 4);

    assert!(grid.scrollback.is_empty());
    assert_eq!(grid.cur_y, 3);
    assert_eq!(grid.saved_cursor, Some((1, 3)));
    let row0: String = grid.lines[0].iter().map(|cell| cell.ch()).collect();
    let row3: String = grid.lines[3].iter().map(|cell| cell.ch()).collect();
    assert_eq!(row0, "aaaa");
    assert_eq!(row3, "dddd");

    feed(&mut grid, &mut sink, b"\x1b[?1049h");
    assert!(grid.is_alt);
    set_line(&mut grid, 0, "1111");
    set_line(&mut grid, 1, "2222");
    set_line(&mut grid, 2, "3333");
    set_line(&mut grid, 3, "4444");
    grid.cur_y = 3;

    grid.resize(6, 2);

    assert_eq!(grid.cols, 6);
    assert_eq!(grid.visible_rows, 2);
    assert!(grid.scrollback.is_empty());
    assert!(grid.lines.iter().all(|line| line.len() == 6));
    let row0: String = grid.lines[0].iter().map(|cell| cell.ch()).collect();
    let row1: String = grid.lines[1].iter().map(|cell| cell.ch()).collect();
    assert_eq!(row0, "3333  ");
    assert_eq!(row1, "4444  ");

    feed(&mut grid, &mut sink, b"\x1b[?1049l");

    assert!(!grid.is_alt);
    assert!(grid.alt_lines.is_none());
    let row0: String = grid.lines[0].iter().map(|cell| cell.ch()).collect();
    let row1: String = grid.lines[1].iter().map(|cell| cell.ch()).collect();
    assert_eq!(row0, "aaaa  ");
    assert_eq!(row1, "bbbb  ");
    assert_eq!(grid.cur_y, 1);
}
