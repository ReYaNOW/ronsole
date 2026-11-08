//! Pure geometry for split panes inside a tab: the split tree, minimum pane
//! sizes and the single integer split formula shared by layout, hit testing
//! and divider dragging. No `App`, terminal or renderer state lives here.

use crate::panels::GRID_TILE_GAP;
use crate::search::Rect;

/// Direction of a split. `SideBySide` places panes along x separated by a
/// vertical divider; `Stacked` places them along y separated by a horizontal
/// divider.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SplitAxis {
    SideBySide,
    Stacked,
}

/// Stable identity of one pane inside a tab's split tree.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub(crate) struct PaneId(u64);

impl PaneId {
    pub(crate) fn new(raw: u64) -> Self {
        Self(raw)
    }

    pub(crate) fn raw(self) -> u64 {
        self.0
    }
}

/// One node of a tab's split tree: a pane or a cut with the saved ratio of
/// the first child. The tree itself never stores layout pixels.
#[derive(Clone, Debug)]
pub(crate) enum SplitNode {
    Leaf(PaneId),
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<SplitNode>,
        second: Box<SplitNode>,
    },
}

/// Minimum text body of one pane: 10 columns by 2 rows plus the text padding.
pub(crate) fn pane_min(char_w: f32, char_h: f32, scale: f32) -> (f32, f32) {
    (10.0 * char_w + 20.0 * scale, 2.0 * char_h + 16.0 * scale)
}

#[inline]
fn axis_extent(rect: Rect, axis: SplitAxis) -> f32 {
    match axis {
        SplitAxis::SideBySide => rect.w,
        SplitAxis::Stacked => rect.h,
    }
}

/// Gap between siblings in pixels, snapped to whole pixels for this scale.
#[inline]
fn node_gap(scale: f32) -> f32 {
    (GRID_TILE_GAP * scale).round()
}

/// Minimum size of a subtree along `axis`. Leaves use [`pane_min`]; every
/// branch is ceiled so the result compares against whole-pixel extents.
pub(crate) fn min_extent(
    node: &SplitNode,
    axis: SplitAxis,
    char_w: f32,
    char_h: f32,
    scale: f32,
) -> f32 {
    match node {
        SplitNode::Leaf(_) => {
            let (w, h) = pane_min(char_w, char_h, scale);
            match axis {
                SplitAxis::SideBySide => w.ceil(),
                SplitAxis::Stacked => h.ceil(),
            }
        }
        SplitNode::Split {
            axis: split_axis,
            first,
            second,
            ..
        } => {
            let first_min = min_extent(first, axis, char_w, char_h, scale).ceil();
            let second_min = min_extent(second, axis, char_w, char_h, scale).ceil();
            if *split_axis == axis {
                first_min + node_gap(scale) + second_min
            } else {
                first_min.max(second_min)
            }
        }
    }
}

/// The only split formula: cut `node` into the `first` and `second` child
/// rects. The saved `ratio` is clamped so both children keep their minimums
/// when the node is large enough; a node too small for both minimums is split
/// by the saved ratio instead. Both extents are whole pixels and, together
/// with the node gap, exactly cover the node.
pub(crate) fn split_child_rects(
    node: Rect,
    axis: SplitAxis,
    ratio: f32,
    gap: f32,
    first_min: f32,
    second_min: f32,
) -> (Rect, Rect) {
    let extent = axis_extent(node, axis);
    let node_gap = if extent < gap { 0.0 } else { gap };
    let available = (extent - node_gap).max(0.0);
    let clamped_ratio = if available > 0.0 {
        let lo = first_min.ceil() / available;
        let hi = 1.0 - second_min.ceil() / available;
        if lo <= hi {
            ratio.clamp(lo, hi)
        } else {
            ratio
        }
    } else {
        ratio
    };
    let first_extent = (available * clamped_ratio).floor();
    let second_extent = available - first_extent;
    match axis {
        SplitAxis::SideBySide => (
            Rect {
                x: node.x,
                y: node.y,
                w: first_extent,
                h: node.h,
            },
            Rect {
                x: node.x + first_extent + node_gap,
                y: node.y,
                w: second_extent,
                h: node.h,
            },
        ),
        SplitAxis::Stacked => (
            Rect {
                x: node.x,
                y: node.y,
                w: node.w,
                h: first_extent,
            },
            Rect {
                x: node.x,
                y: node.y + first_extent + node_gap,
                w: node.w,
                h: second_extent,
            },
        ),
    }
}

/// Lay out every leaf of `root` inside `body` in pre-order first→second.
/// `out` is reused: it is cleared and refilled without per-frame allocation.
pub(crate) fn layout_panes(
    root: &SplitNode,
    body: Rect,
    scale: f32,
    char_w: f32,
    char_h: f32,
    out: &mut Vec<(PaneId, Rect)>,
) {
    out.clear();
    layout_node(root, body, scale, char_w, char_h, out);
}

fn layout_node(
    node: &SplitNode,
    rect: Rect,
    scale: f32,
    char_w: f32,
    char_h: f32,
    out: &mut Vec<(PaneId, Rect)>,
) {
    match node {
        SplitNode::Leaf(id) => out.push((*id, rect)),
        SplitNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let first_min = min_extent(first, *axis, char_w, char_h, scale);
            let second_min = min_extent(second, *axis, char_w, char_h, scale);
            let (first_rect, second_rect) =
                split_child_rects(rect, *axis, *ratio, node_gap(scale), first_min, second_min);
            layout_node(first, first_rect, scale, char_w, char_h, out);
            layout_node(second, second_rect, scale, char_w, char_h, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::terminal_grid_size;
    use crate::search::Rect;

    fn pane(id: u64) -> SplitNode {
        SplitNode::Leaf(PaneId::new(id))
    }

    fn split(axis: SplitAxis, ratio: f32, first: SplitNode, second: SplitNode) -> SplitNode {
        SplitNode::Split {
            axis,
            ratio,
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// Trees with 1, 2, 3, 4 and 6 leaves, including nested mixed axes.
    fn test_trees() -> Vec<(usize, SplitNode)> {
        vec![
            (1, pane(1)),
            (2, split(SplitAxis::SideBySide, 0.5, pane(1), pane(2))),
            (
                3,
                split(
                    SplitAxis::Stacked,
                    0.4,
                    pane(1),
                    split(SplitAxis::SideBySide, 0.5, pane(2), pane(3)),
                ),
            ),
            (
                4,
                split(
                    SplitAxis::SideBySide,
                    0.55,
                    split(SplitAxis::Stacked, 0.5, pane(1), pane(2)),
                    split(SplitAxis::Stacked, 0.6, pane(3), pane(4)),
                ),
            ),
            (
                6,
                split(
                    SplitAxis::Stacked,
                    0.5,
                    split(
                        SplitAxis::SideBySide,
                        0.5,
                        pane(1),
                        split(SplitAxis::SideBySide, 0.5, pane(2), pane(3)),
                    ),
                    split(
                        SplitAxis::SideBySide,
                        0.4,
                        pane(4),
                        split(SplitAxis::Stacked, 0.5, pane(5), pane(6)),
                    ),
                ),
            ),
        ]
    }

    fn intersects(a: Rect, b: Rect) -> bool {
        a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
    }

    /// Gap of a split whose extent along the split axis is `extent`: no gap is
    /// reserved when the node is narrower than the gap itself.
    fn gap_for(scale: f32, extent: f32) -> f32 {
        let gap = (GRID_TILE_GAP * scale).round();
        if extent < gap {
            0.0
        } else {
            gap
        }
    }

    /// Bounding box of the leaves of `node`, consumed from `leaves` in
    /// pre-order first→second. Returns `(bbox, leaf area, gap area)`; the gap
    /// area of a split is its node gap times the node's cross extent.
    fn walk(
        node: &SplitNode,
        leaves: &[(PaneId, Rect)],
        next: &mut usize,
        scale: f32,
    ) -> (Rect, f32, f32) {
        match node {
            SplitNode::Leaf(id) => {
                let (leaf_id, rect) = leaves[*next];
                assert_eq!(leaf_id, *id, "leaves are emitted in pre-order");
                *next += 1;
                (rect, rect.w * rect.h, 0.0)
            }
            SplitNode::Split {
                axis, first, second, ..
            } => {
                let (first_rect, first_area, first_gaps) = walk(first, leaves, next, scale);
                let (second_rect, second_area, second_gaps) = walk(second, leaves, next, scale);
                let x = first_rect.x.min(second_rect.x);
                let y = first_rect.y.min(second_rect.y);
                let right = (first_rect.x + first_rect.w).max(second_rect.x + second_rect.w);
                let bottom = (first_rect.y + first_rect.h).max(second_rect.y + second_rect.h);
                let bbox = Rect {
                    x,
                    y,
                    w: right - x,
                    h: bottom - y,
                };
                let (extent, cross) = match axis {
                    SplitAxis::SideBySide => (bbox.w, bbox.h),
                    SplitAxis::Stacked => (bbox.h, bbox.w),
                };
                let gap_area = gap_for(scale, extent) * cross;
                (
                    bbox,
                    first_area + second_area,
                    first_gaps + second_gaps + gap_area,
                )
            }
        }
    }

    #[test]
    fn layout_covers_body_without_overlap() {
        let char_w = 10.0_f32;
        let char_h = 20.0_f32;
        let body = Rect {
            x: 0.0,
            y: 0.0,
            w: 1600.0,
            h: 1000.0,
        };
        let body_area = body.w * body.h;
        for scale in [1.0_f32, 1.25, 1.5, 2.0] {
            for (leaf_count, tree) in test_trees() {
                let mut leaves = Vec::new();
                layout_panes(&tree, body, scale, char_w, char_h, &mut leaves);
                assert_eq!(leaves.len(), leaf_count, "scale {scale}");
                for (id, rect) in &leaves {
                    assert_eq!(rect.x.fract(), 0.0, "scale {scale} pane {id:?}");
                    assert_eq!(rect.y.fract(), 0.0, "scale {scale} pane {id:?}");
                    assert_eq!(rect.w.fract(), 0.0, "scale {scale} pane {id:?}");
                    assert_eq!(rect.h.fract(), 0.0, "scale {scale} pane {id:?}");
                    assert!(rect.x >= body.x && rect.y >= body.y, "scale {scale}");
                    assert!(rect.x + rect.w <= body.x + body.w, "scale {scale}");
                    assert!(rect.y + rect.h <= body.y + body.h, "scale {scale}");
                }
                for i in 0..leaves.len() {
                    for j in i + 1..leaves.len() {
                        assert!(
                            !intersects(leaves[i].1, leaves[j].1),
                            "scale {scale} panes {:?} and {:?} overlap",
                            leaves[i].0,
                            leaves[j].0
                        );
                    }
                }
                let mut next = 0;
                let (bbox, leaf_area, gap_area) = walk(&tree, &leaves, &mut next, scale);
                assert_eq!(next, leaves.len(), "scale {scale}");
                assert_eq!(bbox, body, "scale {scale}");
                assert_eq!(
                    leaf_area + gap_area,
                    body_area,
                    "scale {scale}, {leaf_count} leaves: panes plus gaps must cover the body"
                );
            }
        }
    }

    #[test]
    fn layout_restores_ratios_after_shrink() {
        let tree = split(SplitAxis::SideBySide, 0.6, pane(1), pane(2));
        let (char_w, char_h, scale) = (10.0_f32, 20.0_f32, 1.0_f32);
        assert_eq!(pane_min(char_w, char_h, scale), (120.0, 56.0));

        let wide = Rect {
            x: 0.0,
            y: 0.0,
            w: 1200.0,
            h: 800.0,
        };
        let mut wide_out = Vec::new();
        layout_panes(&tree, wide, scale, char_w, char_h, &mut wide_out);
        let wide_first = wide_out[0].1;
        let wide_second = wide_out[1].1;
        // available = 1200 - 6 = 1194; first = floor(1194 * 0.6) = 716.
        assert_eq!(wide_first.w, 716.0);
        assert_eq!(wide_second.w, 478.0);
        assert_eq!(wide_second.x, 722.0);
        assert!(wide_first.w > wide_second.w);

        let small = Rect {
            x: 0.0,
            y: 0.0,
            w: 300.0,
            h: 120.0,
        };
        let mut small_out = Vec::new();
        layout_panes(&tree, small, scale, char_w, char_h, &mut small_out);
        let small_first = small_out[0].1;
        let small_second = small_out[1].1;
        // available = 294; clamped to hi = 1 - 120 / 294 = 0.5918.
        assert!(small_first.w >= 120.0, "first pane keeps its minimum");
        assert!(small_second.w >= 120.0, "second pane keeps its minimum");
        assert!(
            small_first.w < 176.0,
            "ratio was clamped: unclamped floor(294 * 0.6) = 176"
        );
        assert_eq!(small_first.w + small_second.w, 294.0);
        assert_eq!(small_second.x, small_first.x + small_first.w + 6.0);

        let mut restored = Vec::new();
        layout_panes(&tree, wide, scale, char_w, char_h, &mut restored);
        assert_eq!(restored, wide_out);
    }

    #[test]
    fn layout_tiny_body_is_nonnegative() {
        let tree = split(SplitAxis::SideBySide, 0.6, pane(1), pane(2));
        for (w, h) in [(100.0_f32, 40.0), (4.0, 40.0), (300.0, 120.0)] {
            for scale in [1.0_f32, 1.5, 2.0] {
                let body = Rect {
                    x: 10.0,
                    y: 20.0,
                    w,
                    h,
                };
                let mut out = Vec::new();
                layout_panes(&tree, body, scale, 10.0, 20.0, &mut out);
                let first = out[0].1;
                let second = out[1].1;
                for rect in [first, second] {
                    assert!(rect.w >= 0.0, "w {w} h {h} scale {scale}");
                    assert!(rect.h >= 0.0, "w {w} h {h} scale {scale}");
                    assert!(rect.x >= body.x && rect.y >= body.y);
                    assert!(rect.x + rect.w <= body.x + body.w);
                    assert!(rect.y + rect.h <= body.y + body.h);
                }
                assert_eq!(first.x, body.x);
                assert_eq!(first.h, body.h);
                assert_eq!(second.y, body.y);
                assert_eq!(second.h, body.h);
                assert_eq!(first.w + gap_for(scale, w) + second.w, w);
                assert_eq!(second.x, body.x + first.w + gap_for(scale, w));
            }
        }
    }

    #[test]
    fn pane_min_matches_terminal_grid_size() {
        let char_w = 9.3_f32;
        let char_h = 21.7_f32;
        for scale in [1.0_f32, 1.333, 2.0] {
            let (w, h) = pane_min(char_w, char_h, scale);
            let base = Rect {
                x: 0.0,
                y: 0.0,
                w,
                h,
            };
            let size = terminal_grid_size(base, char_w, char_h, scale);
            assert!(
                size.raw_cols >= 10.0 - 0.001,
                "scale {scale}: raw_cols {}",
                size.raw_cols
            );
            assert!(
                size.raw_rows >= 2.0 - 0.001,
                "scale {scale}: raw_rows {}",
                size.raw_rows
            );
            let narrow = terminal_grid_size(
                Rect {
                    x: base.x,
                    y: base.y,
                    w: w - 1.0,
                    h,
                },
                char_w,
                char_h,
                scale,
            );
            assert!(
                narrow.raw_cols < 10.0 - 0.001,
                "scale {scale}: raw_cols {}",
                narrow.raw_cols
            );
            let short = terminal_grid_size(
                Rect {
                    x: base.x,
                    y: base.y,
                    w,
                    h: h - 1.0,
                },
                char_w,
                char_h,
                scale,
            );
            assert!(
                short.raw_rows < 2.0 - 0.001,
                "scale {scale}: raw_rows {}",
                short.raw_rows
            );
        }
    }
}
