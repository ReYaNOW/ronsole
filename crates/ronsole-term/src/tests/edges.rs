//! Terminal edge-case tests moved from the Ronsole terminal module.

use crate::cell::{Cell, UnderlineStyle};
use crate::color::TerminalColor;
use crate::input::MouseTrackingMode;
use crate::grid::{default_tab_stops, repair_wide_line};
use crate::term::Term;
use super::support::{
    RecordingSink, assert_line_metadata_invariant, assert_wide_line_invariant, feed,
    logical_texts, set_line, test_config,
};

#[test]
fn terminal_csi_defaults_scroll_modes_and_sgr_edges_end_to_end() {
    let mut grid = Term::new(test_config(6, 3));
    let mut sink = RecordingSink::new();

    feed(&mut grid, &mut sink, b"\t\x08\x08Z");
    assert_eq!((grid.cur_x, grid.cur_y), (4, 0));
    assert_eq!(grid.lines[0][3].ch(), 'Z');

    feed(&mut grid, &mut sink, b"\x1b[0G");
    assert_eq!(grid.cur_x, 0);
    feed(&mut grid, &mut sink, b"\x1b[99G");
    assert_eq!(grid.cur_x, 5);
    feed(&mut grid, &mut sink, b"\x1b[0d");
    assert_eq!(grid.cur_y, 0);
    feed(&mut grid, &mut sink, b"\x1b[99d");
    assert_eq!(grid.cur_y, 2);
    feed(&mut grid, &mut sink, b"\x1b[0;0f");
    assert_eq!((grid.cur_x, grid.cur_y), (0, 0));

    feed(
        &mut grid, &mut sink,
        b"\x1b[m\x1b[1;34;104mA\x1b[39;49mB\x1b[38;2;1;2;3;48;2;4;5;6mC\x1b[90;107mD\x1b[0mE\x1b[38;5;13;48;5;6mF",
    );
    assert_eq!(
        (grid.lines[0][0].ch(), grid.lines[0][0].fg(), grid.lines[0][0].bg()),
        ('A', TerminalColor::indexed(12), TerminalColor::indexed(12))
    );
    assert_eq!(
        (grid.lines[0][1].ch(), grid.lines[0][1].fg(), grid.lines[0][1].bg()),
        ('B', TerminalColor::default_foreground(), TerminalColor::default_background())
    );
    assert_eq!(grid.lines[0][2].ch(), 'C');
    assert_eq!(grid.lines[0][2].fg().rgb_value(), Some((1, 2, 3)));
    assert_eq!(grid.lines[0][2].bg().rgb_value(), Some((4, 5, 6)));
    assert_eq!(
        (grid.lines[0][3].ch(), grid.lines[0][3].fg(), grid.lines[0][3].bg()),
        ('D', TerminalColor::indexed(8), TerminalColor::indexed(15))
    );
    assert_eq!(
        (grid.lines[0][4].ch(), grid.lines[0][4].fg(), grid.lines[0][4].bg()),
        ('E', TerminalColor::default_foreground(), TerminalColor::default_background())
    );
    assert_eq!(
        (grid.lines[0][5].ch(), grid.lines[0][5].fg(), grid.lines[0][5].bg()),
        ('F', TerminalColor::indexed(13), TerminalColor::indexed(6))
    );
    assert!(!grid.cur_bold);

    feed(&mut grid, &mut sink, b"\x1b[?25l\x1b[?1h\x1b[?1002h");
    assert!(!grid.cursor_visible);
    assert!(grid.app_cursor_keys);
    assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::ButtonMotion);
    feed(&mut grid, &mut sink, b"\x1b[?1006h");
    assert!(grid.mouse_sgr);
    feed(&mut grid, &mut sink, b"\x1b[?25h\x1b[?1l\x1b[?1006l\x1b[?1002l");
    assert!(grid.cursor_visible);
    assert!(!grid.app_cursor_keys);
    assert!(!grid.mouse_sgr);
    assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::None);

    let mut scroll = Term::new(test_config(4, 3));
    let mut scroll_sink = RecordingSink::new();
    set_line(&mut scroll, 0, "aaaa");
    set_line(&mut scroll, 1, "bbbb");
    set_line(&mut scroll, 2, "cccc");

    feed(&mut scroll, &mut scroll_sink, b"\x1b[2;3r\x1b[S");
    let row0: String = scroll.lines[0].iter().map(|cell| cell.ch()).collect();
    let row1: String = scroll.lines[1].iter().map(|cell| cell.ch()).collect();
    let row2: String = scroll.lines[2].iter().map(|cell| cell.ch()).collect();
    assert_eq!(row0, "aaaa");
    assert_eq!(row1, "cccc");
    assert_eq!(row2, "    ");

    feed(&mut scroll, &mut scroll_sink, b"\x1b[T");
    let row1: String = scroll.lines[1].iter().map(|cell| cell.ch()).collect();
    let row2: String = scroll.lines[2].iter().map(|cell| cell.ch()).collect();
    assert_eq!(row1, "    ");
    assert_eq!(row2, "cccc");
}

#[test]
fn terminal_osc_insert_delete_and_truecolor_edges_end_to_end() {
    let mut grid = Term::new(test_config(8, 4));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"\x1b]10;?\x1b\\");
    assert_eq!(sink.bytes, b"\x1B]10;rgb:ffff/ffff/ffff\x1B\\".to_vec());

    feed(&mut grid, &mut sink, b"\x1b[2;1Habcdefgh\x1b[2;3H\x1b[2P");
    let row1: String = grid.lines[1].iter().map(|cell| cell.ch()).collect();
    assert_eq!(row1, "abefgh  ");

    feed(&mut grid, &mut sink, b"\x1b[2;2H\x1b[3X");
    let row1: String = grid.lines[1].iter().map(|cell| cell.ch()).collect();
    assert_eq!(row1, "a   gh  ");

    set_line(&mut grid, 1, "11111111");
    set_line(&mut grid, 2, "22222222");
    set_line(&mut grid, 3, "33333333");
    feed(&mut grid, &mut sink, b"\x1b[2;4r\x1b[3;1H\x1b[L");
    assert_eq!(
        grid.lines[2].iter().map(|cell| cell.ch()).collect::<String>(),
        "        "
    );
    assert_eq!(
        grid.lines[3].iter().map(|cell| cell.ch()).collect::<String>(),
        "22222222"
    );

    feed(&mut grid, &mut sink, b"\x1b[2;1H\x1b[M");
    assert_eq!(
        grid.lines[1].iter().map(|cell| cell.ch()).collect::<String>(),
        "        "
    );

    feed(&mut grid, &mut sink, b"\x1b[38;2;1;2;3m\x1b[48;5;42mX\x1b[39;49m");
    assert!(grid.cur_bg.is_default_background());
    assert!(grid.cur_fg.is_default_foreground());
    assert_eq!(grid.lines[1][0].bg(), 42);
    assert_eq!(grid.lines[1][0].fg().rgb_value(), Some((1, 2, 3)));

    let before = (grid.cols, grid.visible_rows, grid.lines.len());
    grid.resize(before.0, before.1);
    assert_eq!((grid.cols, grid.visible_rows, grid.lines.len()), before);

    grid.scroll_region = (2, 2);
    grid.scroll_region_up(1);
    grid.scroll_region_down(1);
    assert_eq!(grid.scroll_region, (2, 2));
}

#[test]
fn terminal_resize_selection_and_alt_growth_edges() {
    let mut grid = Term::new(test_config(3, 2));
    let mut sink = RecordingSink::new();
    set_line(&mut grid, 0, "abc");
    set_line(&mut grid, 1, "def");
    let mut scrollback_cell = Cell::default();
    scrollback_cell.set_ch('s');
    grid.push_scrollback_row_for_test(vec![scrollback_cell; 3], 3, false);
    grid.saved_cursor = Some((2, 1));

    grid.resize(3, 4);
    assert_eq!(grid.visible_rows, 4);
    assert_eq!(grid.cur_y, 1);
    assert_eq!(grid.saved_cursor, Some((2, 2)));
    assert!(grid.scrollback.is_empty());
    assert_eq!(
        grid.lines[0].iter().map(|cell| cell.ch()).collect::<String>(),
        "sss"
    );

    grid.selection = Some((2, 2, 1, 0));
    assert_eq!(grid.selection_text(), "ss\nabc\ndef");
    grid.selection = Some((0, 99, 2, 100));
    assert_eq!(grid.selection_text(), "");
    grid.selection = None;
    assert_eq!(grid.selection_text(), "");

    feed(&mut grid, &mut sink, b"\x1b[?1049h");
    assert!(grid.is_alt);
    grid.alt_saved_cursor = Some((9, 9));
    grid.resize(5, 5);
    assert_eq!(grid.alt_saved_cursor, Some((2, 3)));
    assert_eq!(grid.lines.len(), 5);
    assert!(grid.lines.iter().all(|line| line.len() == 5));
    grid.resize(5, 3);
    assert_eq!(grid.lines.len(), 3);
    assert_eq!(grid.visible_rows, 3);
}

#[test]
fn terminal_csi_more_erase_cursor_and_reply_edges() {
    let mut grid = Term::new(test_config(5, 3));
    let mut sink = RecordingSink::new();
    set_line(&mut grid, 0, "abcde");
    set_line(&mut grid, 1, "fghij");
    set_line(&mut grid, 2, "klmno");

    feed(&mut grid, &mut sink, b"\x1b[2;3H\x1b[J");
    assert_eq!(
        grid.lines[1].iter().map(|cell| cell.ch()).collect::<String>(),
        "fg   "
    );
    assert_eq!(
        grid.lines[2].iter().map(|cell| cell.ch()).collect::<String>(),
        "     "
    );

    set_line(&mut grid, 0, "abcde");
    set_line(&mut grid, 1, "fghij");
    set_line(&mut grid, 2, "klmno");
    feed(&mut grid, &mut sink, b"\x1b[2;3H\x1b[1J");
    assert_eq!(
        grid.lines[0].iter().map(|cell| cell.ch()).collect::<String>(),
        "     "
    );
    assert_eq!(
        grid.lines[1].iter().map(|cell| cell.ch()).collect::<String>(),
        "   ij"
    );

    set_line(&mut grid, 1, "fghij");
    feed(&mut grid, &mut sink, b"\x1b[2;3H\x1b[2K");
    assert_eq!(
        grid.lines[1].iter().map(|cell| cell.ch()).collect::<String>(),
        "     "
    );

    feed(&mut grid, &mut sink, b"\x1b[1;1H\x1b[10C\x1b[2D\x1b[2B\x1b[A");
    assert_eq!((grid.cur_x, grid.cur_y), (2, 1));

    feed(&mut grid, &mut sink, b"\x1b7\x1b[3;5H\x1b8");
    assert_eq!((grid.cur_x, grid.cur_y), (2, 1));

    feed(&mut grid, &mut sink, b"\x1b]11;?\x1b\\");
    assert_eq!(sink.bytes, b"\x1B]11;rgb:ffff/ffff/ffff\x1B\\".to_vec());

    // The sink accumulates every reply, so the second check covers both.
    feed(&mut grid, &mut sink, b"\x1b]10;?\x07");
    assert_eq!(
        sink.bytes,
        b"\x1B]11;rgb:ffff/ffff/ffff\x1B\\\x1B]10;rgb:ffff/ffff/ffff\x1B\\".to_vec()
    );

    feed(&mut grid, &mut sink, b"\x1b[38;5;123m\x1b[48;2;9;8;7mQ\x1b[999m");
    assert_eq!(grid.lines[1][2].ch(), 'Q');
    assert_eq!(grid.lines[1][2].fg(), 123);
    assert_eq!(grid.lines[1][2].bg().rgb_value(), Some((9, 8, 7)));
}

#[test]
fn wide_combining_and_common_emoji_clusters_preserve_terminal_columns() {
    let mut grid = Term::new(test_config(24, 2));
    let mut sink = RecordingSink::new();
    let text = "A界e\u{0301}👍🏽👨\u{200D}👩\u{200D}👧X";
    feed(&mut grid, &mut sink, text.as_bytes());

    assert_eq!(grid.cur_x, 9);
    assert!(grid.lines[0][1].is_wide());
    assert!(grid.lines[0][2].is_wide_spacer());
    assert_eq!(grid.lines[0][3].zero_width(), &['\u{0301}']);
    assert!(grid.lines[0][4].is_wide());
    assert!(grid.lines[0][5].is_wide_spacer());
    assert_eq!(grid.lines[0][4].zero_width(), &['🏽']);
    assert!(grid.lines[0][6].is_wide());
    assert!(grid.lines[0][7].is_wide_spacer());
    assert_eq!(
        grid.lines[0][6].zero_width(),
        &['\u{200D}', '👩', '\u{200D}', '👧']
    );

    grid.selection = Some((0, 0, 8, 0));
    assert_eq!(grid.selection_text(), text);

    feed(&mut grid, &mut sink, b"\x1b[1;3HZ");
    assert_eq!(grid.lines[0][1].ch(), ' ');
    assert!(!grid.lines[0][1].is_wide());
    assert_eq!(grid.lines[0][2].ch(), 'Z');
    assert!(!grid.lines[0][2].is_wide_spacer());
}

#[test]
fn partial_wide_mutations_never_leave_orphan_bases_or_spacers() {
    for sequence in [
        b"\x1b[1;2H\x1b[X".as_slice(),
        b"\x1b[1;2H\x1b[P".as_slice(),
        b"\x1b[1;2H\x1b[@".as_slice(),
        b"\x1b[1;2H\x1b[4hZ\x1b[4l".as_slice(),
        b"\x1b[1;2H\x1b[K".as_slice(),
        b"\x1b[1;2H\x1b[J".as_slice(),
    ] {
        let mut grid = Term::new(test_config(4, 1));
        let mut sink = RecordingSink::new();
        feed(&mut grid, &mut sink, "界X".as_bytes());
        feed(&mut grid, &mut sink, sequence);
        assert_wide_line_invariant(&grid.lines[0]);
        assert_ne!(grid.lines[0][0].ch(), '界');
    }

    let mut resized = Term::new(test_config(4, 1));
    let mut resized_sink = RecordingSink::new();
    feed(&mut resized, &mut resized_sink, "AB界".as_bytes());
    assert!(resized.lines[0][2].is_wide());
    assert!(resized.lines[0][3].is_wide_spacer());
    resized.resize(3, 1);
    assert_wide_line_invariant(&resized.lines[0]);
    assert_eq!(resized.lines[0][2].ch(), ' ');
}

#[test]
fn wide_line_repair_blanks_broken_footprints_instead_of_downgrading_width() {
    let mut orphan_base = vec![Cell::default(); 2];
    orphan_base[0].set_char(
        '界',
        TerminalColor::default_foreground(),
        TerminalColor::default_background(),
        true,
    );
    repair_wide_line(&mut orphan_base);
    assert_eq!(orphan_base[0].ch(), ' ');
    assert_wide_line_invariant(&orphan_base);

    let mut orphan_spacer = vec![Cell::default(); 2];
    orphan_spacer[1].set_wide_spacer(
        TerminalColor::default_foreground(),
        TerminalColor::default_background(),
    );
    repair_wide_line(&mut orphan_spacer);
    assert_eq!(orphan_spacer[1].ch(), ' ');
    assert_wide_line_invariant(&orphan_spacer);
}

#[test]
fn width_two_glyph_at_right_edge_never_degrades_to_one_cell() {
    let mut no_wrap = Term::new(test_config(4, 2));
    let mut no_wrap_sink = RecordingSink::new();
    feed(&mut no_wrap, &mut no_wrap_sink, b"\x1b[?7l\x1b[1;4H");
    feed(&mut no_wrap, &mut no_wrap_sink, "界".as_bytes());
    assert_eq!((no_wrap.cur_x, no_wrap.cur_y), (3, 0));
    assert!(!no_wrap.lines[0].iter().any(|cell| cell.ch() == '界'));
    assert_wide_line_invariant(&no_wrap.lines[0]);

    let mut wrap = Term::new(test_config(4, 2));
    let mut wrap_sink = RecordingSink::new();
    wrap.lines[0][3].set_ch('Q');
    feed(&mut wrap, &mut wrap_sink, b"\x1b[41m\x1b[1;4H");
    feed(&mut wrap, &mut wrap_sink, "界".as_bytes());
    assert_eq!(wrap.lines[0][3].ch(), ' ');
    assert_eq!(wrap.lines[0][3].bg(), TerminalColor::indexed(1));
    assert_eq!(wrap.lines[1][0].ch(), '界');
    assert!(wrap.lines[1][0].is_wide());
    assert!(wrap.lines[1][1].is_wide_spacer());
    assert_wide_line_invariant(&wrap.lines[0]);
    assert_wide_line_invariant(&wrap.lines[1]);

    let mut one_col = Term::new(test_config(1, 2));
    let mut one_col_sink = RecordingSink::new();
    feed(&mut one_col, &mut one_col_sink, "界".as_bytes());
    assert!(!one_col.lines.iter().flatten().any(|cell| cell.ch() == '界'));
    for line in &one_col.lines {
        assert_wide_line_invariant(line);
    }
}

#[test]
fn linefeed_below_decstbm_region_is_physical_bottom_noop() {
    let mut grid = Term::new(test_config(5, 6));
    let mut sink = RecordingSink::new();
    for (row, text) in ["00000", "11111", "22222", "33333", "44444", "55555"]
        .into_iter()
        .enumerate()
    {
        set_line(&mut grid, row, text);
    }
    feed(&mut grid, &mut sink, b"\x1b[3;5r\x1b[6;1H");
    let before = grid.lines.clone();
    feed(&mut grid, &mut sink, b"\n");
    assert_eq!(grid.lines, before);
    assert_eq!(grid.cur_y, 5);

    feed(&mut grid, &mut sink, b"\x1b[5;1H\n");
    assert_eq!(grid.lines[0].iter().map(|cell| cell.ch()).collect::<String>(), "00000");
    assert_eq!(grid.lines[1].iter().map(|cell| cell.ch()).collect::<String>(), "11111");
    assert_eq!(grid.lines[2].iter().map(|cell| cell.ch()).collect::<String>(), "33333");
    assert_eq!(grid.lines[3].iter().map(|cell| cell.ch()).collect::<String>(), "44444");
    assert_eq!(grid.lines[4].iter().map(|cell| cell.ch()).collect::<String>(), "     ");
    assert_eq!(grid.lines[5].iter().map(|cell| cell.ch()).collect::<String>(), "55555");
    assert_eq!(grid.cur_y, 4);

    let region_after_scroll = grid.lines.clone();
    feed(&mut grid, &mut sink, b"\x1b[2;1H\n");
    assert_eq!(grid.lines, region_after_scroll);
    assert_eq!(grid.cur_y, 2);
}

#[test]
fn esc_ind_uses_same_decstbm_linefeed_semantics() {
    let mut grid = Term::new(test_config(5, 6));
    let mut sink = RecordingSink::new();
    for (row, text) in ["00000", "11111", "22222", "33333", "44444", "55555"]
        .into_iter()
        .enumerate()
    {
        set_line(&mut grid, row, text);
    }
    feed(&mut grid, &mut sink, b"\x1b[3;5r\x1b[6;1H");
    let before = grid.lines.clone();
    feed(&mut grid, &mut sink, b"\x1bD");
    assert_eq!(grid.lines, before);
    assert_eq!(grid.cur_y, 5);

    feed(&mut grid, &mut sink, b"\x1b[5;1H\x1bD");
    assert_eq!(grid.lines[2].iter().map(|cell| cell.ch()).collect::<String>(), "33333");
    assert_eq!(grid.lines[3].iter().map(|cell| cell.ch()).collect::<String>(), "44444");
    assert_eq!(grid.lines[4].iter().map(|cell| cell.ch()).collect::<String>(), "     ");
    assert_eq!(grid.cur_y, 4);
}

#[test]
fn bce_erase_insert_delete_char_paths_use_current_background() {
    let red = TerminalColor::indexed(1);

    let mut erase_display = Term::new(test_config(5, 2));
    let mut erase_display_sink = RecordingSink::new();
    set_line(&mut erase_display, 0, "ABCDE");
    set_line(&mut erase_display, 1, "FGHIJ");
    feed(&mut erase_display, &mut erase_display_sink, b"\x1b[41m\x1b[2J");
    assert!(erase_display.lines.iter().flatten().all(|cell| cell.ch() == ' ' && cell.bg() == red));

    let mut erase_line = Term::new(test_config(5, 1));
    let mut erase_line_sink = RecordingSink::new();
    set_line(&mut erase_line, 0, "ABCDE");
    feed(&mut erase_line, &mut erase_line_sink, b"\x1b[41m\x1b[1;3H\x1b[K");
    assert_eq!(erase_line.lines[0][1].ch(), 'B');
    assert!(erase_line.lines[0][2..].iter().all(|cell| cell.ch() == ' ' && cell.bg() == red));

    let mut ech = Term::new(test_config(5, 1));
    let mut ech_sink = RecordingSink::new();
    set_line(&mut ech, 0, "ABCDE");
    feed(&mut ech, &mut ech_sink, b"\x1b[41m\x1b[1;2H\x1b[2X");
    assert!(ech.lines[0][1..3].iter().all(|cell| cell.ch() == ' ' && cell.bg() == red));

    let mut dch = Term::new(test_config(5, 1));
    let mut dch_sink = RecordingSink::new();
    set_line(&mut dch, 0, "ABCDE");
    feed(&mut dch, &mut dch_sink, b"\x1b[41m\x1b[1;2H\x1b[P");
    assert_eq!(dch.lines[0].iter().map(|cell| cell.ch()).collect::<String>(), "ACDE ");
    assert_eq!(dch.lines[0][4].bg(), red);

    let mut ich = Term::new(test_config(5, 1));
    let mut ich_sink = RecordingSink::new();
    set_line(&mut ich, 0, "ABCDE");
    feed(&mut ich, &mut ich_sink, b"\x1b[41m\x1b[1;2H\x1b[@");
    assert_eq!(ich.lines[0][1].ch(), ' ');
    assert_eq!(ich.lines[0][1].bg(), red);
}

#[test]
fn bce_line_insert_delete_and_scroll_blanks_use_current_background() {
    let red = TerminalColor::indexed(1);

    let mut il = Term::new(test_config(4, 3));
    let mut il_sink = RecordingSink::new();
    for (row, text) in ["1111", "2222", "3333"].into_iter().enumerate() {
        set_line(&mut il, row, text);
    }
    feed(&mut il, &mut il_sink, b"\x1b[41m\x1b[2;1H\x1b[L");
    assert!(il.lines[1].iter().all(|cell| cell.ch() == ' ' && cell.bg() == red));

    let mut dl = Term::new(test_config(4, 3));
    let mut dl_sink = RecordingSink::new();
    for (row, text) in ["1111", "2222", "3333"].into_iter().enumerate() {
        set_line(&mut dl, row, text);
    }
    feed(&mut dl, &mut dl_sink, b"\x1b[41m\x1b[2;1H\x1b[M");
    assert!(dl.lines[2].iter().all(|cell| cell.ch() == ' ' && cell.bg() == red));

    let mut scroll = Term::new(test_config(4, 3));
    let mut scroll_sink = RecordingSink::new();
    for (row, text) in ["1111", "2222", "3333"].into_iter().enumerate() {
        set_line(&mut scroll, row, text);
    }
    feed(&mut scroll, &mut scroll_sink, b"\x1b[41m\x1b[3;1H\n");
    assert!(scroll.lines[2].iter().all(|cell| cell.ch() == ' ' && cell.bg() == red));
    assert_eq!(scroll.scrollback.len(), 1);
}

#[test]
fn sgr_intensity_preserves_default_and_explicit_bright_identity() {
    let mut grid = Term::new(test_config(8, 1));
    let mut sink = RecordingSink::new();
    feed(
        &mut grid, &mut sink,
        b"\x1b[1mA\x1b[31;1mB\x1b[22mC\x1b[91mD\x1b[22mE\x1b[1;91mF",
    );
    assert!(grid.lines[0][0].fg().is_default_foreground());
    assert_eq!(grid.lines[0][1].fg(), TerminalColor::indexed(9));
    assert_eq!(grid.lines[0][2].fg(), TerminalColor::indexed(1));
    assert_eq!(grid.lines[0][3].fg(), TerminalColor::indexed(9));
    assert_eq!(grid.lines[0][4].fg(), TerminalColor::indexed(9));
    assert_eq!(grid.lines[0][5].fg(), TerminalColor::indexed(9));
}

#[test]
fn sgr_inverse_underline_dim_and_reset_are_stored_on_cells_and_wide_spacers() {
    let mut grid = Term::new(test_config(12, 1));
    let mut sink = RecordingSink::new();
    feed(
        &mut grid, &mut sink,
        b"\x1b[7mA\x1b[27mB\x1b[4mC\x1b[24mD\x1b[2mE\x1b[22mF\x1b[2;4;7m",
    );
    feed(&mut grid, &mut sink, "界".as_bytes());
    feed(&mut grid, &mut sink, b"\x1b[0mG");
    assert!(grid.lines[0][0].is_inverse());
    assert!(!grid.lines[0][1].is_inverse());
    assert!(grid.lines[0][2].underline() != UnderlineStyle::None);
    assert!(grid.lines[0][3].underline() == UnderlineStyle::None);
    assert!(grid.lines[0][4].is_dim());
    assert!(!grid.lines[0][5].is_dim());
    assert!(grid.lines[0][6].is_wide());
    assert!(grid.lines[0][6].is_inverse());
    assert!(grid.lines[0][6].underline() != UnderlineStyle::None);
    assert!(grid.lines[0][6].is_dim());
    assert!(grid.lines[0][7].is_wide_spacer());
    assert!(grid.lines[0][7].is_inverse());
    assert!(grid.lines[0][7].underline() != UnderlineStyle::None);
    assert!(grid.lines[0][7].is_dim());
    assert!(!grid.lines[0][8].is_inverse());
    assert!(grid.lines[0][8].underline() == UnderlineStyle::None);
    assert!(!grid.lines[0][8].is_dim());
    assert_wide_line_invariant(&grid.lines[0]);
}

#[test]
fn sgr_colon_underline_reset_does_not_leave_a_blank_cell_decoration() {
    let mut grid = Term::new(test_config(4, 1));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"\x1b[4mA\x1b[4:0m B");

    assert_eq!(grid.lines[0][0].ch(), 'A');
    assert!(grid.lines[0][0].underline() != UnderlineStyle::None);
    assert_eq!(grid.lines[0][1].ch(), ' ');
    assert!(grid.lines[0][1].underline() == UnderlineStyle::None);
    assert_eq!(grid.lines[0][2].ch(), 'B');
    assert!(grid.lines[0][2].underline() == UnderlineStyle::None);
}

#[test]
fn column_reflow_clears_selection_and_copy_uses_reflowed_rows() {
    let mut grid = Term::new(test_config(13, 1));
    let mut row = vec![Cell::default(); 13];
    for (cell, c) in row.iter_mut().zip("hit....SECRET".chars()) {
        cell.set_ch(c);
    }
    grid.push_scrollback_row_for_test(row, 13, false);
    grid.selection = Some((7, 0, 12, 0));

    grid.resize(7, 1);

    assert!(grid.selection.is_none());
    assert_eq!(grid.scrollback.len(), 2);
    assert!(grid.scrollback.iter().all(|row| row.len() == 7));
    assert_eq!(logical_texts(&grid)[0], "hit....SECRET");
    grid.selection = Some((0, 0, 2, 0));
    assert_eq!(grid.selection_text(), "hit");
    grid.selection = Some((0, 1, 5, 1));
    assert_eq!(grid.selection_text(), "SECRET");
    assert_line_metadata_invariant(&grid);
}

#[test]
fn bce_wide_mutations_blank_broken_footprints_with_current_background() {
    let red = TerminalColor::indexed(1);
    let mut erase = Term::new(test_config(4, 1));
    let mut erase_sink = RecordingSink::new();
    feed(&mut erase, &mut erase_sink, "界X".as_bytes());
    feed(&mut erase, &mut erase_sink, b"\x1b[41m\x1b[1;2H\x1b[X");
    assert_eq!(erase.lines[0][0].ch(), ' ');
    assert_eq!(erase.lines[0][1].ch(), ' ');
    assert_eq!(erase.lines[0][0].bg(), red);
    assert_eq!(erase.lines[0][1].bg(), red);
    assert_wide_line_invariant(&erase.lines[0]);

    let mut overwrite = Term::new(test_config(4, 1));
    let mut overwrite_sink = RecordingSink::new();
    feed(&mut overwrite, &mut overwrite_sink, "界X".as_bytes());
    feed(&mut overwrite, &mut overwrite_sink, b"\x1b[41m\x1b[1;2HZ");
    assert_eq!(overwrite.lines[0][0].ch(), ' ');
    assert_eq!(overwrite.lines[0][0].bg(), red);
    assert_eq!(overwrite.lines[0][1].ch(), 'Z');
    assert_eq!(overwrite.lines[0][1].bg(), red);
    assert_wide_line_invariant(&overwrite.lines[0]);
}

#[test]
fn common_dec_modes_cover_wrap_origin_insert_tabs_mouse_focus_and_paste() {
    let mut grid = Term::new(test_config(20, 4));
    let mut sink = RecordingSink::new();

    feed(&mut grid, &mut sink, b"\t");
    assert_eq!(grid.cur_x, 8);
    feed(&mut grid, &mut sink, b"\x1b[I");
    assert_eq!(grid.cur_x, 16);
    feed(&mut grid, &mut sink, b"\x1b[Z");
    assert_eq!(grid.cur_x, 8);
    feed(&mut grid, &mut sink, b"\x1b[3g\r\t");
    assert_eq!(grid.cur_x, 19);
    feed(&mut grid, &mut sink, b"\r\x1b[5G\x1bH\r\t");
    assert_eq!(grid.cur_x, 4);

    feed(&mut grid, &mut sink, b"\x1b[2;4r\x1b[?6h\x1b[2;3H");
    assert_eq!((grid.cur_x, grid.cur_y), (2, 2));
    feed(&mut grid, &mut sink, b"\x1b[?6l");
    assert_eq!((grid.cur_x, grid.cur_y), (0, 0));

    feed(&mut grid, &mut sink, b"abc\x1b[1;2H\x1b[4hX\x1b[4l");
    assert_eq!(
        grid.lines[0][..4].iter().map(|cell| cell.ch()).collect::<String>(),
        "aXbc"
    );

    feed(
        &mut grid, &mut sink,
        b"\x1b[?1002h\x1b[?1006h\x1b[?1004h\x1b[?2004h",
    );
    assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::ButtonMotion);
    assert!(grid.mouse_sgr);
    assert!(grid.focus_reporting);
    assert!(grid.bracketed_paste);
    feed(&mut grid, &mut sink, b"\x1b[?1003h");
    assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::AnyMotion);
    feed(&mut grid, &mut sink, b"\x1b[?1003l");
    assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::ButtonMotion);
    feed(&mut grid, &mut sink, b"\x1b[?1002l\x1b[?1006l\x1b[?1004l\x1b[?2004l");
    assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::None);
    assert!(!grid.mouse_sgr);
    assert!(!grid.focus_reporting);
    assert!(!grid.bracketed_paste);

    let mut wrap = Term::new(test_config(3, 2));
    let mut wrap_sink = RecordingSink::new();
    feed(&mut wrap, &mut wrap_sink, b"\x1b[?7l\x1b[1;3HAB");
    assert_eq!(wrap.lines[0][2].ch(), 'B');
    assert_eq!((wrap.cur_x, wrap.cur_y), (2, 0));
    feed(&mut wrap, &mut wrap_sink, b"\x1b[?7h\x1b[1;3HCD");
    assert_eq!(wrap.lines[0][2].ch(), 'C');
    assert_eq!(wrap.lines[1][0].ch(), 'D');
    assert_eq!((wrap.cur_x, wrap.cur_y), (1, 1));
}

fn assert_terminal_modes_reset(grid: &Term) {
    assert!(grid.cur_fg.is_default_foreground());
    assert!(grid.cur_bg.is_default_background());
    assert!(!grid.cur_bold);
    assert!(!grid.cur_dim);
    assert_eq!(grid.cur_underline, UnderlineStyle::None);
    assert!(!grid.cur_inverse);
    assert!(!grid.insert_mode);
    assert!(!grid.origin_mode);
    assert!(grid.autowrap);
    assert!(!grid.wrap_pending);
    assert!(!grid.join_next);
    assert!(!grid.app_cursor_keys);
    assert!(grid.cursor_visible);
    assert_eq!(grid.mouse_tracking_mode, MouseTrackingMode::None);
    assert_eq!(grid.mouse_tracking_mask, 0);
    assert!(!grid.mouse_sgr);
    assert!(!grid.bracketed_paste);
    assert!(!grid.focus_reporting);
    assert_eq!(grid.scroll_region, (0, grid.visible_rows.saturating_sub(1)));
    assert!(grid.saved_cursor.is_none());
    assert!(grid.alt_saved_cursor.is_none());
}

#[test]
fn ris_hard_reset_restores_startup_terminal_state_without_rehiding_presentation() {
    let mut grid = Term::new(test_config(16, 4));
    let mut sink = RecordingSink::new();
    grid.mark_presentation_ready();
    grid.mark_presentation_layout_ready();
    grid.push_scrollback_row_for_test(vec![Cell::default(); 16], 0, false);
    grid.selection = Some((0, 0, 2, 0));
    feed(
        &mut grid, &mut sink,
        b"\x1b[31;44;1;2;4;7m\x1b[4h\x1b[2;3r\x1b[?6h\x1b[?7l\x1b[?1h\x1b[?25l\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[2;4H\x1b[s\x1bH\x1b[?1049hALT",
    );
    assert!(grid.is_alt);
    assert!(grid.presentation_visible());
    assert!(!grid.scrollback.is_empty());

    feed(&mut grid, &mut sink, b"\x1bc");

    assert!(!grid.is_alt);
    assert!(grid.alt_lines.is_none());
    assert!(grid.scrollback.is_empty());
    assert!(grid.selection.is_none());
    assert_eq!((grid.cur_x, grid.cur_y), (0, 0));
    assert!(grid
        .lines
        .iter()
        .flatten()
        .all(|cell| cell.ch() == ' ' && cell.bg().is_default_background()));
    assert_terminal_modes_reset(&grid);
    assert_eq!(grid.tab_stops, default_tab_stops(grid.cols));
    assert!(grid.presentation_ready);
    assert!(grid.presentation_layout_ready);
    assert!(grid.presentation_visible());
    assert_eq!(grid.scrollback_storage_cols, grid.cols);
}

#[test]
fn decstr_soft_reset_preserves_content_and_history_but_resets_mutable_modes() {
    let mut grid = Term::new(test_config(16, 4));
    let mut sink = RecordingSink::new();
    let mut history = vec![Cell::default(); 16];
    history[0].set_ch('H');
    grid.push_scrollback_row_for_test(history, 1, false);
    feed(
        &mut grid, &mut sink,
        b"VISIBLE\x1b[31;44;1;2;4;7m\x1b[4h\x1b[2;3r\x1b[?6h\x1b[?7l\x1b[?1h\x1b[?25l\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[3;7H\x1b[s",
    );
    let before_lines = grid.lines.clone();
    let before_history = grid.scrollback.clone();
    let before_cursor = (grid.cur_x, grid.cur_y);

    feed(&mut grid, &mut sink, b"\x1b[!p");

    assert_eq!(grid.lines, before_lines);
    assert_eq!(grid.scrollback, before_history);
    assert_eq!((grid.cur_x, grid.cur_y), before_cursor);
    assert_terminal_modes_reset(&grid);
}

#[test]
fn local_xterm_256color_tput_reset_sequence_resets_supported_state() {
    const XTERM_256COLOR_RESET: &[u8] =
        b"\x1bc\x1b]104\x07\x1b[!p\x1b[?3;4l\x1b[4l\x1b>\x1b[?69l";
    let mut grid = Term::new(test_config(16, 4));
    let mut sink = RecordingSink::new();
    grid.push_scrollback_row_for_test(vec![Cell::default(); 16], 0, false);
    feed(
        &mut grid, &mut sink,
        b"dirty\x1b[31;44;1;2;4;7m\x1b[4h\x1b[2;3r\x1b[?6h\x1b[?7l\x1b[?1h\x1b[?25l\x1b[?1003h\x1b[?1006h\x1b[?1004h\x1b[?2004h\x1b[?1049hALT",
    );

    feed(&mut grid, &mut sink, XTERM_256COLOR_RESET);

    assert_terminal_modes_reset(&grid);
    assert!(!grid.is_alt);
    assert!(grid.scrollback.is_empty());
    assert_eq!((grid.cur_x, grid.cur_y), (0, 0));
    assert!(grid.lines.iter().flatten().all(|cell| cell.ch() == ' '));
}

#[test]
fn hostile_csi_counts_match_maximum_meaningful_geometry() {
    let cases: &[(&[u8], &[u8])] = &[
        (b"\x1b[65535P", b"\x1b[7P"),
        (b"\x1b[65535L", b"\x1b[3L"),
        (b"\x1b[65535M", b"\x1b[3M"),
        (b"\x1b[65535S", b"\x1b[4S"),
        (b"\x1b[65535T", b"\x1b[4T"),
        (b"\x1b[65535I", b"\x1b[8I"),
        (b"\x1b[65535Z", b"\x1b[8Z"),
    ];
    for &(hostile_sequence, maximum_sequence) in cases {
        let mut hostile_grid = Term::new(test_config(8, 4));
        let mut hostile_grid_sink = RecordingSink::new();
        let mut maximum_grid = Term::new(test_config(8, 4));
        let mut maximum_grid_sink = RecordingSink::new();
        for (grid, sink) in [
            (&mut hostile_grid, &mut hostile_grid_sink),
            (&mut maximum_grid, &mut maximum_grid_sink),
        ] {
            for (row, text) in ["abcdefgh", "ijklmnop", "qrstuvwx", "yzABCDEF"]
                .into_iter()
                .enumerate()
            {
                set_line(grid, row, text);
            }
            feed(grid, sink, b"\x1b[41m\x1b[2;2H");
        }
        feed(&mut hostile_grid, &mut hostile_grid_sink, hostile_sequence);
        feed(&mut maximum_grid, &mut maximum_grid_sink, maximum_sequence);
        assert_eq!(
            hostile_grid.lines, maximum_grid.lines,
            "sequence {hostile_sequence:?}"
        );
        assert_eq!(
            (hostile_grid.cur_x, hostile_grid.cur_y),
            (maximum_grid.cur_x, maximum_grid.cur_y),
            "sequence {hostile_sequence:?}"
        );
        for line in &hostile_grid.lines {
            assert_wide_line_invariant(line);
        }
    }
}
