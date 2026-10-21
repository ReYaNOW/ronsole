use super::support::{
    RecordingSink, assert_line_metadata_invariant, assert_wide_line_invariant, feed,
    logical_cell_lines, logical_texts, set_line, test_config,
};
use crate::cell::{CELL_PRESENTATION_AUTO, Cell, UnderlineStyle};
use crate::color::TerminalColor;
use crate::grid::LineMeta;
use crate::term::Term;

#[test]
fn terminal_grid_print_scroll_resize_and_selection_end_to_end() {
    let mut grid = Term::new(test_config(4, 2));
    let mut sink = RecordingSink::new();
    for ch in "abcd".chars() {
        grid.put_char(ch);
    }
    grid.put_char('e');

    assert_eq!(grid.lines[0][0].ch(), 'a');
    assert_eq!(grid.lines[0][3].ch(), 'd');
    assert_eq!(grid.lines[1][0].ch(), 'e');

    grid.newline();
    feed(&mut grid, &mut sink, b"\r");
    grid.put_char('z');
    assert_eq!(grid.scrollback.len(), 1);
    assert_eq!(grid.lines[1][0].ch(), 'z');

    grid.selection = Some((0, 2, 0, 2));
    assert_eq!(grid.selection_text(), "z");

    grid.resize(6, 3);
    assert_eq!(grid.cols, 6);
    assert_eq!(grid.visible_rows, 3);
    assert!(grid.lines.iter().all(|line| line.len() == 6));
}

#[test]
fn terminal_scrollback_and_line_pool_remain_bounded() {
    let mut grid = Term::new(test_config(2, 2));
    for _ in 0..10_512 {
        grid.newline();
    }

    assert_eq!(grid.scrollback.len(), 10_000);
    assert!(grid.pool.len() <= 128);
}

#[test]
fn major_column_shrink_drops_obsolete_visible_line_capacity() {
    let mut grid = Term::new(test_config(4096, 2));
    let before = grid.lines[0].capacity();
    grid.resize(64, 2);
    assert!(grid.lines.iter().all(|line| line.len() == 64));
    assert!(grid.lines.iter().all(|line| line.capacity() < before));
}

#[test]
fn scrollback_reflow_releases_obsolete_row_capacity() {
    let mut grid = Term::new(test_config(4096, 1));
    grid.push_scrollback_row_for_test(vec![Cell::default(); 4096], 4096, false);
    let before_capacity = grid.scrollback[0].capacity();

    grid.resize(64, 1);

    assert_eq!(grid.scrollback.len(), 64);
    assert!(grid.scrollback.iter().all(|row| row.len() == 64));
    assert!(
        grid.scrollback
            .iter()
            .all(|row| row.capacity() < before_capacity && row.capacity() <= 128)
    );
    assert_eq!(grid.scrollback_storage_cols, 64);
    assert_line_metadata_invariant(&grid);
}

#[test]
fn repeated_column_reflow_keeps_scrollback_storage_width_bounded() {
    let mut grid = Term::new(test_config(4096, 1));
    let mut history = vec![Cell::default(); 4096];
    history[0].set_ch('A');
    history[79].set_ch('Z');
    grid.push_scrollback_row_for_test(history, 80, false);

    for width in [3000, 2000, 1200, 700, 400, 240, 140, 80] {
        grid.resize(width, 1);
        assert!(grid.scrollback.iter().all(|row| row.len() == width));
        assert!(
            grid.scrollback
                .iter()
                .all(|row| { row.capacity() <= width.saturating_mul(2).max(128) })
        );
        assert_eq!(grid.scrollback_storage_cols, width);
        assert_line_metadata_invariant(&grid);
    }

    let logical = logical_cell_lines(&grid);
    assert_eq!(logical[0][0].ch(), 'A');
    assert_eq!(logical[0][79].ch(), 'Z');
}

#[test]
fn minor_column_shrink_reflows_scrollback_without_hidden_tail_storage() {
    let mut grid = Term::new(test_config(120, 1));
    let mut history = vec![Cell::default(); 120];
    history[0].set_ch('A');
    history[119].set_ch('Z');
    grid.push_scrollback_row_for_test(history, 120, false);

    grid.resize(119, 1);

    assert_eq!(grid.scrollback.len(), 2);
    assert!(grid.scrollback.iter().all(|row| row.len() == 119));
    let logical = logical_cell_lines(&grid);
    assert_eq!(logical[0][0].ch(), 'A');
    assert_eq!(logical[0][119].ch(), 'Z');
    assert_line_metadata_invariant(&grid);
}

#[test]
fn horizontal_resize_shrink_reflows_soft_wrapped_logical_line() {
    let mut grid = Term::new(test_config(6, 4));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"abcdefghij");
    assert_eq!(logical_texts(&grid)[0], "abcdefghij");

    grid.resize(4, 4);

    assert_eq!(logical_texts(&grid)[0], "abcdefghij");
    assert_eq!(
        grid.line_meta[0],
        LineMeta {
            soft_wrapped: true,
            used_cols: 4
        }
    );
    assert_eq!(
        grid.line_meta[1],
        LineMeta {
            soft_wrapped: true,
            used_cols: 4
        }
    );
    assert_eq!(
        grid.line_meta[2],
        LineMeta {
            soft_wrapped: false,
            used_cols: 2
        }
    );
    assert_eq!((grid.cur_x, grid.cur_y), (2, 2));
    assert_line_metadata_invariant(&grid);
}

#[test]
fn horizontal_resize_widen_unwraps_soft_wrapped_logical_line() {
    let mut grid = Term::new(test_config(4, 3));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"abcdefgh");
    assert!(grid.line_meta[0].soft_wrapped);
    assert!(grid.wrap_pending);

    grid.resize(10, 3);

    assert_eq!(logical_texts(&grid)[0], "abcdefgh");
    assert_eq!(
        grid.line_meta[0],
        LineMeta {
            soft_wrapped: false,
            used_cols: 8
        }
    );
    assert_eq!((grid.cur_x, grid.cur_y), (8, 0));
    assert!(!grid.wrap_pending);
    assert_line_metadata_invariant(&grid);
}

#[test]
fn horizontal_resize_preserves_hard_newline_boundaries() {
    let mut grid = Term::new(test_config(4, 4));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"abcd\r\nefgh");
    assert!(!grid.line_meta[0].soft_wrapped);

    grid.resize(10, 4);

    let logical = logical_texts(&grid);
    assert_eq!(logical[0], "abcd");
    assert_eq!(logical[1], "efgh");
    assert!(!grid.line_meta[0].soft_wrapped);
    assert!(!grid.line_meta[1].soft_wrapped);
    assert_line_metadata_invariant(&grid);
}

#[test]
fn horizontal_resize_round_trip_preserves_cell_state_and_unicode_attachments() {
    let mut grid = Term::new(test_config(8, 5));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"\x1b[31;44;1;2;4;7m");
    feed(&mut grid, &mut sink, "A✔\u{FE0F}e\u{0301}界BCDEFGH".as_bytes());
    let before = logical_cell_lines(&grid)[0].clone();
    assert!(before.iter().any(Cell::is_wide));
    assert!(before.iter().any(|cell| !cell.zero_width().is_empty()));
    assert!(
        before
            .iter()
            .any(|cell| cell.presentation() != CELL_PRESENTATION_AUTO)
    );
    assert!(before.iter().any(Cell::is_inverse));
    assert!(before.iter().any(|cell| cell.underline() != UnderlineStyle::None));
    assert!(before.iter().any(Cell::is_dim));

    grid.resize(4, 5);
    for row in grid.scrollback.iter().chain(grid.lines.iter()) {
        assert_wide_line_invariant(row);
    }
    grid.resize(12, 5);

    assert_eq!(logical_cell_lines(&grid)[0], before);
    for row in grid.scrollback.iter().chain(grid.lines.iter()) {
        assert_wide_line_invariant(row);
    }
    assert_line_metadata_invariant(&grid);
}

#[test]
fn horizontal_resize_preserves_background_only_cells() {
    let red = TerminalColor::indexed(1);
    let mut grid = Term::new(test_config(4, 2));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"\x1b[41m\x1b[2J");
    assert!(grid.lines.iter().flatten().all(|cell| cell.bg() == red));

    grid.resize(2, 2);

    let logical = logical_cell_lines(&grid);
    assert_eq!(logical.len(), 2);
    assert!(logical.iter().all(|line| line.len() == 4));
    assert!(logical.iter().flatten().all(|cell| cell.bg() == red));
    assert_line_metadata_invariant(&grid);
}

#[test]
fn horizontal_resize_keeps_wide_glyph_footprint_together_at_boundary() {
    let mut grid = Term::new(test_config(6, 4));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, "abc界Z".as_bytes());

    grid.resize(4, 4);

    assert_eq!(grid.lines[0][0].ch(), 'a');
    assert_eq!(grid.lines[0][2].ch(), 'c');
    assert_eq!(grid.lines[0][3].ch(), ' ');
    assert!(grid.lines[1][0].is_wide());
    assert!(grid.lines[1][1].is_wide_spacer());
    assert_eq!(grid.lines[1][2].ch(), 'Z');
    assert_eq!(grid.line_meta[0].used_cols, 3);
    assert!(grid.line_meta[0].soft_wrapped);
    for row in &grid.lines {
        assert_wide_line_invariant(row);
    }
    assert_eq!(logical_texts(&grid)[0], "abc界Z");
}

#[test]
fn horizontal_resize_maps_cursor_and_wrap_pending_to_new_width() {
    let mut grid = Term::new(test_config(4, 3));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"abcd");
    assert_eq!((grid.cur_x, grid.cur_y, grid.wrap_pending), (3, 0, true));

    grid.resize(2, 3);
    assert_eq!((grid.cur_x, grid.cur_y, grid.wrap_pending), (1, 1, true));
    assert_eq!(logical_texts(&grid)[0], "abcd");

    grid.resize(8, 3);
    assert_eq!((grid.cur_x, grid.cur_y, grid.wrap_pending), (4, 0, false));
    assert_eq!(logical_texts(&grid)[0], "abcd");
}

#[test]
fn horizontal_resize_reflows_logical_line_across_scrollback_boundary() {
    let mut grid = Term::new(test_config(4, 2));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"abcdefghi");
    assert_eq!(grid.scrollback.len(), 1);
    assert!(grid.scrollback_meta[0].soft_wrapped);

    grid.resize(3, 2);

    assert_eq!(logical_texts(&grid)[0], "abcdefghi");
    assert_eq!(grid.scrollback.len(), 1);
    assert!(grid.scrollback_meta[0].soft_wrapped);
    assert!(grid.line_meta[0].soft_wrapped);
    assert!(!grid.line_meta[1].soft_wrapped);
    assert_line_metadata_invariant(&grid);
}

#[test]
fn horizontal_resize_does_not_join_hard_scrollback_boundary() {
    let mut grid = Term::new(test_config(4, 2));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"abcd\r\nefgh\r\ni");
    assert_eq!(grid.scrollback.len(), 1);
    assert!(!grid.scrollback_meta[0].soft_wrapped);

    grid.resize(8, 2);

    let logical = logical_texts(&grid);
    assert_eq!(logical[0], "abcd");
    assert_eq!(logical[1], "efgh");
    assert_eq!(logical[2], "i");
    assert_eq!(grid.scrollback.len(), 1);
    assert!(!grid.scrollback_meta[0].soft_wrapped);
}

#[test]
fn alt_screen_column_resize_reflows_hidden_normal_history_only() {
    let mut grid = Term::new(test_config(4, 3));
    let mut sink = RecordingSink::new();
    feed(&mut grid, &mut sink, b"abcdefgh");
    feed(&mut grid, &mut sink, b"\x1b[?1049hALT");
    assert!(grid.is_alt);

    grid.resize(8, 3);

    assert!(grid.is_alt);
    assert_eq!(
        grid.lines[0]
            .iter()
            .take(3)
            .map(|cell| cell.ch())
            .collect::<String>(),
        "ALT"
    );
    assert!(grid.lines.iter().all(|line| line.len() == 8));
    feed(&mut grid, &mut sink, b"\x1b[?1049l");

    assert!(!grid.is_alt);
    assert_eq!(logical_texts(&grid)[0], "abcdefgh");
    assert_eq!(grid.line_meta[0].used_cols, 8);
    assert!(!grid.line_meta[0].soft_wrapped);
    assert_line_metadata_invariant(&grid);
}

#[test]
fn custom_scroll_region_column_resize_keeps_conservative_physical_rows() {
    let mut grid = Term::new(test_config(6, 3));
    set_line(&mut grid, 0, "abcdef");
    set_line(&mut grid, 1, "ghijkl");
    set_line(&mut grid, 2, "mnopqr");
    grid.scroll_region = (1, 2);

    grid.resize(8, 3);

    assert_eq!(
        grid.lines[0].iter().map(|cell| cell.ch()).collect::<String>(),
        "abcdef  "
    );
    assert_eq!(
        grid.lines[1].iter().map(|cell| cell.ch()).collect::<String>(),
        "ghijkl  "
    );
    assert_eq!(
        grid.lines[2].iter().map(|cell| cell.ch()).collect::<String>(),
        "mnopqr  "
    );
    assert!(grid.scrollback.is_empty());
    assert_line_metadata_invariant(&grid);
}
