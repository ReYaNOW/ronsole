//! Pure geometry for grid-mode panels: tile layout, Q9 fit check and hit
//! testing. Constants are logical pixels, multiplied by the window scale at
//! call time. Every boundary is snapped to whole pixels with `.round()`.

use crate::renderer::terminal_grid_size;
use crate::search::Rect;

/// Gap between neighbouring tiles, in logical pixels.
pub(crate) const GRID_TILE_GAP: f32 = 6.0;
/// Height of a tile header, in logical pixels.
pub(crate) const GRID_TILE_HEADER: f32 = 22.0;

/// One visible terminal panel. `header` is `None` in ordinary (non-grid)
/// mode and `Some` for a grid tile.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Panel {
    pub terminal: usize,
    pub body: Rect,
    pub header: Option<Rect>,
    pub content_hidden: bool,
}

/// Result of [`panel_hit`]: the index refers to the `panels` slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelHit {
    Header(usize),
    Body(usize),
}

/// Fills `out` with `(header, body)` pairs for `n` tiles inside `body`, in
/// tab order. `out` is cleared first so the caller can reuse its allocation.
pub(crate) fn grid_tiles(body: Rect, n: usize, scale: f32, out: &mut Vec<(Rect, Rect)>) {
    out.clear();
    for index in 0..n {
        out.push(grid_tile(body, n, scale, index));
    }
}

/// Returns `true` when every tile body has at least 10 columns and 2 rows,
/// judged on the unclamped grid size. `n == 0` always fits.
pub(crate) fn grid_fits(body: Rect, n: usize, scale: f32, char_w: f32, char_h: f32) -> bool {
    (0..n).all(|index| {
        let (_, tile_body) = grid_tile(body, n, scale, index);
        let size = terminal_grid_size(tile_body, char_w, char_h, scale);
        size.raw_cols >= 10.0 && size.raw_rows >= 2.0
    })
}

/// Finds which panel header or body contains `(x, y)`. Gaps between panels
/// return `None`.
pub(crate) fn panel_hit(panels: &[Panel], x: f32, y: f32) -> Option<PanelHit> {
    for (index, panel) in panels.iter().enumerate() {
        if panel.header.is_some_and(|header| header.contains(x, y)) {
            return Some(PanelHit::Header(index));
        }
        if panel.body.contains(x, y) {
            return Some(PanelHit::Body(index));
        }
    }
    None
}

/// Header and body rectangles of tile `index` in an `n`-tile grid. Requires
/// `index < n`. Allocation-free so grid-mode callers can check every tile
/// without a scratch buffer.
fn grid_tile(body: Rect, n: usize, scale: f32, index: usize) -> (Rect, Rect) {
    // cols = ceil(sqrt(n)), found by search so no float rounding is involved.
    let cols = (1_usize..).find(|c| c * c >= n).unwrap_or(n);
    let rows = n.div_ceil(cols);
    let gap = (GRID_TILE_GAP * scale).round();
    let header_h = (GRID_TILE_HEADER * scale).round();
    let (col, row) = (index % cols, index / cols);
    let left = grid_edge(body.x, body.w, gap, col, cols);
    let right = grid_edge(body.x, body.w, gap, col + 1, cols) - gap;
    let top = grid_edge(body.y, body.h, gap, row, rows);
    let bottom = grid_edge(body.y, body.h, gap, row + 1, rows) - gap;
    let tile_h = bottom - top;
    let header_h = header_h.min(tile_h);
    let header = Rect {
        x: left,
        y: top,
        w: right - left,
        h: header_h,
    };
    let tile_body = Rect {
        x: left,
        y: top + header_h,
        w: right - left,
        h: tile_h - header_h,
    };
    (header, tile_body)
}

/// Snapped leading edge of slot `k` out of `count` slots that split
/// `start..start + extent` with `gap` between neighbours. The edge of slot
/// `count` is the trailing edge of the whole span plus `gap`, so a boundary
/// minus `gap` gives the trailing edge of the previous slot.
fn grid_edge(start: f32, extent: f32, gap: f32, k: usize, count: usize) -> f32 {
    (start + (k as f32) * (extent + gap) / (count as f32)).round()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    /// Full tile rectangle (header plus body) for one returned pair.
    fn tile_bounds(pair: &(Rect, Rect)) -> Rect {
        let (header, body) = *pair;
        rect(header.x, header.y, header.w, body.y + body.h - header.y)
    }

    fn overlaps(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    #[test]
    fn grid_tiles_cover_body_without_overlap() {
        let body = rect(0.0, 40.0, 1000.0, 700.0);
        let mut out = Vec::new();
        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            let gap = (GRID_TILE_GAP * scale).round();
            let header_h = (GRID_TILE_HEADER * scale).round();
            for n in 1..=9_usize {
                grid_tiles(body, n, scale, &mut out);
                assert_eq!(out.len(), n, "scale {scale} n {n}");
                let cols = (1_usize..).find(|c| c * c >= n).unwrap();
                let rows = n.div_ceil(cols);
                for (i, pair) in out.iter().enumerate() {
                    let (header, tb) = *pair;
                    for v in [
                        header.x, header.y, header.w, header.h, tb.x, tb.y, tb.w, tb.h,
                    ] {
                        assert_eq!(v.fract(), 0.0, "scale {scale} n {n} tile {i}");
                    }
                    assert_eq!(header.h, header_h, "scale {scale} n {n} tile {i}");
                    assert_eq!(tb.y, header.y + header_h, "scale {scale} n {n} tile {i}");
                    assert_eq!(tb.x, header.x, "scale {scale} n {n} tile {i}");
                    assert_eq!(tb.w, header.w, "scale {scale} n {n} tile {i}");

                    let t = tile_bounds(pair);
                    assert!(
                        t.x >= body.x && t.x + t.w <= body.x + body.w,
                        "scale {scale} n {n} tile {i}"
                    );
                    assert!(
                        t.y >= body.y && t.y + t.h <= body.y + body.h,
                        "scale {scale} n {n} tile {i}"
                    );
                    for (j, other) in out.iter().enumerate().skip(i + 1) {
                        assert!(
                            !overlaps(t, tile_bounds(other)),
                            "scale {scale} n {n} tiles {i} {j}"
                        );
                    }

                    let (col, row) = (i % cols, i / cols);
                    if col > 0 {
                        let left = tile_bounds(&out[i - 1]);
                        assert_eq!(t.x - (left.x + left.w), gap, "scale {scale} n {n} tile {i}");
                    } else {
                        assert_eq!(t.x, body.x, "scale {scale} n {n} tile {i}");
                    }
                    if row > 0 {
                        let up = tile_bounds(&out[i - cols]);
                        assert_eq!(t.y - (up.y + up.h), gap, "scale {scale} n {n} tile {i}");
                        assert_eq!(t.x, up.x, "scale {scale} n {n} tile {i}");
                        assert_eq!(t.w, up.w, "scale {scale} n {n} tile {i}");
                    } else {
                        assert_eq!(t.y, body.y, "scale {scale} n {n} tile {i}");
                    }
                    if col == cols - 1 {
                        assert_eq!(t.x + t.w, body.x + body.w, "scale {scale} n {n} tile {i}");
                    }
                    if row == rows - 1 {
                        assert_eq!(t.y + t.h, body.y + body.h, "scale {scale} n {n} tile {i}");
                    }
                }
            }
        }
    }

    #[test]
    fn grid_tiles_three_panels_left_aligned_last_row() {
        let body = rect(0.0, 40.0, 1000.0, 700.0);
        let mut out = Vec::new();
        grid_tiles(body, 3, 1.0, &mut out);
        assert_eq!(out.len(), 3);
        let (h0, b0) = out[0];
        let (h1, _) = out[1];
        let (h2, b2) = out[2];
        assert_eq!(h0.y, h1.y);
        assert_eq!(h2.x, h0.x);
        assert_eq!(h2.w, h0.w);
        assert_eq!(b2.w, b0.w);
        assert_eq!(h0.w, 497.0);
        assert_eq!(h1.x, h0.x + h0.w + 6.0);
        assert!(h2.y > h0.y);
    }

    #[test]
    fn grid_tiles_single_panel_fills_body_below_header() {
        let body = rect(0.0, 40.0, 1000.0, 700.0);
        let mut out = Vec::new();
        grid_tiles(body, 9, 1.0, &mut out);
        assert_eq!(out.len(), 9);
        // `out` is reused: a later call must replace its contents.
        grid_tiles(body, 1, 1.0, &mut out);
        assert_eq!(out.len(), 1);
        let (header, tb) = out[0];
        assert_eq!(header, rect(0.0, 40.0, 1000.0, 22.0));
        assert_eq!(tb, rect(0.0, 62.0, 1000.0, 678.0));
    }

    #[test]
    fn grid_fits_uses_raw_size_before_clamps() {
        // Single tile: its body is the whole body minus the 22 px header.
        // At scale 1 with char_w 10 and char_h 20 the text viewport is h - 16.
        let ok = rect(0.0, 40.0, 120.0, 78.0);
        assert!(grid_fits(ok, 1, 1.0, 10.0, 20.0));
        // raw_cols = (115 - 20) / 10 = 9.5: below 10 before the clamp.
        assert!(!grid_fits(rect(0.0, 40.0, 115.0, 78.0), 1, 1.0, 10.0, 20.0));
        // raw_rows = (54 - 16) / 20 = 1.9: below 2 before the clamp.
        assert!(!grid_fits(rect(0.0, 40.0, 120.0, 76.0), 1, 1.0, 10.0, 20.0));
        // Empty grid always fits.
        assert!(grid_fits(rect(0.0, 40.0, 1.0, 1.0), 0, 1.0, 10.0, 20.0));
    }

    #[test]
    fn panel_hit_selects_header_body_and_gap() {
        let panels = [
            Panel {
                terminal: 0,
                body: rect(0.0, 62.0, 100.0, 100.0),
                header: Some(rect(0.0, 40.0, 100.0, 22.0)),
                content_hidden: false,
            },
            Panel {
                terminal: 1,
                body: rect(106.0, 62.0, 100.0, 100.0),
                header: Some(rect(106.0, 40.0, 100.0, 22.0)),
                content_hidden: false,
            },
        ];
        assert_eq!(panel_hit(&panels, 10.0, 50.0), Some(PanelHit::Header(0)));
        assert_eq!(panel_hit(&panels, 10.0, 100.0), Some(PanelHit::Body(0)));
        assert_eq!(panel_hit(&panels, 110.0, 50.0), Some(PanelHit::Header(1)));
        assert_eq!(panel_hit(&panels, 150.0, 100.0), Some(PanelHit::Body(1)));
        // Gap between the two panels.
        assert_eq!(panel_hit(&panels, 103.0, 100.0), None);
        // Above all panels.
        assert_eq!(panel_hit(&panels, 50.0, 30.0), None);
    }
}
