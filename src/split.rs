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
    let first_floor = first_min.ceil();
    let second_floor = second_min.ceil();
    let mut clamped = false;
    let clamped_ratio = if available > 0.0 {
        let lo = first_floor / available;
        let hi = 1.0 - second_floor / available;
        if lo <= hi {
            clamped = true;
            ratio.clamp(lo, hi)
        } else {
            ratio
        }
    } else {
        ratio
    };
    let mut first_extent = (available * clamped_ratio).floor();
    // `available * lo` can land just below `first_floor` in `f32`, so the
    // floor may undershoot a minimum even though both minima fit.
    if clamped && available >= first_floor + second_floor {
        first_extent = first_extent.clamp(first_floor, available - second_floor);
    }
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
            let (first_rect, second_rect) =
                child_rects(rect, *axis, *ratio, first, second, scale, char_w, char_h);
            layout_node(first, first_rect, scale, char_w, char_h, out);
            layout_node(second, second_rect, scale, char_w, char_h, out);
        }
    }
}

/// Child rects of one `Split` laid out in `rect`. Layout and divider hit
/// testing share this call so the split formula and the minimums are never
/// duplicated.
fn child_rects(
    rect: Rect,
    axis: SplitAxis,
    ratio: f32,
    first: &SplitNode,
    second: &SplitNode,
    scale: f32,
    char_w: f32,
    char_h: f32,
) -> (Rect, Rect) {
    let first_min = min_extent(first, axis, char_w, char_h, scale);
    let second_min = min_extent(second, axis, char_w, char_h, scale);
    split_child_rects(rect, axis, ratio, node_gap(scale), first_min, second_min)
}

/// Direction of the keyboard focus move inside one tab.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PaneDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Number of panes below `node`.
pub(crate) fn leaf_count(node: &SplitNode) -> usize {
    match node {
        SplitNode::Leaf(_) => 1,
        SplitNode::Split { first, second, .. } => leaf_count(first) + leaf_count(second),
    }
}

/// Path of `pane` from the root: `0` is the first branch, `1` the second and
/// the root itself is the empty path. Allocates, so it is not a frame path.
pub(crate) fn find_path(root: &SplitNode, pane: PaneId) -> Option<Vec<u8>> {
    let mut path = Vec::new();
    find_path_in(root, pane, &mut path).then_some(path)
}

fn find_path_in(node: &SplitNode, pane: PaneId, path: &mut Vec<u8>) -> bool {
    match node {
        SplitNode::Leaf(id) => *id == pane,
        SplitNode::Split { first, second, .. } => {
            path.push(0);
            if find_path_in(first, pane, path) {
                return true;
            }
            path.pop();
            path.push(1);
            if find_path_in(second, pane, path) {
                return true;
            }
            path.pop();
            false
        }
    }
}

pub(crate) fn node_at_path<'a>(root: &'a SplitNode, path: &[u8]) -> Option<&'a SplitNode> {
    let mut node = root;
    for step in path {
        node = match node {
            SplitNode::Split { first, second, .. } => match *step {
                0 => first.as_ref(),
                1 => second.as_ref(),
                _ => return None,
            },
            SplitNode::Leaf(_) => return None,
        };
    }
    Some(node)
}

pub(crate) fn node_at_path_mut<'a>(
    root: &'a mut SplitNode,
    path: &[u8],
) -> Option<&'a mut SplitNode> {
    let mut node = root;
    for step in path {
        node = match node {
            SplitNode::Split { first, second, .. } => match *step {
                0 => first.as_mut(),
                1 => second.as_mut(),
                _ => return None,
            },
            SplitNode::Leaf(_) => return None,
        };
    }
    Some(node)
}

/// Replaces `Leaf(pane)` with a split that keeps `pane` first and adds `new`
/// second. Returns `false` when `pane` is not a leaf of the tree.
pub(crate) fn split_leaf_at(
    root: &mut SplitNode,
    pane: PaneId,
    axis: SplitAxis,
    new: PaneId,
) -> bool {
    let Some(path) = find_path(root, pane) else {
        return false;
    };
    let Some(node) = node_at_path_mut(root, &path) else {
        return false;
    };
    let SplitNode::Leaf(id) = node else {
        return false;
    };
    let existing = *id;
    *node = SplitNode::Split {
        axis,
        ratio: 0.5,
        first: Box::new(SplitNode::Leaf(existing)),
        second: Box::new(SplitNode::Leaf(new)),
    };
    true
}

/// Removes `Leaf(pane)` and replaces its parent `Split` with the surviving
/// branch. Returns `false` for a root leaf and for a pane that is not in the
/// tree.
pub(crate) fn remove_leaf(root: &mut SplitNode, pane: PaneId) -> bool {
    let Some(path) = find_path(root, pane) else {
        return false;
    };
    let Some((removed, parent_path)) = path.split_last() else {
        return false;
    };
    let Some(parent) = node_at_path_mut(root, parent_path) else {
        return false;
    };
    collapse_child(parent, *removed, pane)
}

/// Replaces the `Split` at `parent` with the child that does not sit at
/// `removed`. The surviving `Box` is moved into place, never copied.
fn collapse_child(parent: &mut SplitNode, removed: u8, removed_pane: PaneId) -> bool {
    // The placeholder names the pane that is being removed; it is overwritten
    // in both arms and never observed.
    match std::mem::replace(parent, SplitNode::Leaf(removed_pane)) {
        SplitNode::Split { first, second, .. } => {
            *parent = if removed == 0 { *second } else { *first };
            true
        }
        leaf => {
            *parent = leaf;
            false
        }
    }
}

/// Pane that takes the focus when `pane` disappears: the nearest leaf before
/// it in pre-order, otherwise the leaf right after it. `pane` itself is never
/// returned.
pub(crate) fn successor(root: &SplitNode, pane: PaneId) -> Option<PaneId> {
    let mut previous = None;
    let mut found = false;
    let mut next = None;
    successor_walk(root, pane, &mut previous, &mut found, &mut next);
    if found {
        previous.or(next)
    } else {
        None
    }
}

fn successor_walk(
    node: &SplitNode,
    pane: PaneId,
    previous: &mut Option<PaneId>,
    found: &mut bool,
    next: &mut Option<PaneId>,
) {
    match node {
        SplitNode::Leaf(id) => {
            if *found {
                // The first leaf after the target closes the search.
                next.get_or_insert(*id);
            } else if *id == pane {
                *found = true;
            } else {
                *previous = Some(*id);
            }
        }
        SplitNode::Split { first, second, .. } => {
            successor_walk(first, pane, previous, found, next);
            if next.is_none() {
                successor_walk(second, pane, previous, found, next);
            }
        }
    }
}

/// Pane next to `from` in `dir`. Panes whose rect shares a perpendicular
/// interval with the source win; among them the nearest along the axis, then
/// the larger overlap, then the smaller `PaneId`. Without any overlap the
/// nearest centre wins.
pub(crate) fn pane_in_direction(
    rects: &[(PaneId, Rect)],
    from: PaneId,
    dir: PaneDirection,
) -> Option<PaneId> {
    let active = rects
        .iter()
        .find_map(|(id, rect)| (*id == from).then_some(*rect))?;
    let mut overlapping: Option<(f32, f32, PaneId)> = None;
    let mut centered: Option<(f32, PaneId)> = None;
    for &(id, rect) in rects {
        if id == from {
            continue;
        }
        let (in_direction, distance, overlap) = match dir {
            PaneDirection::Left => {
                let edge = rect.x + rect.w;
                (
                    edge <= active.x,
                    active.x - edge,
                    interval_overlap(rect.y, rect.h, active.y, active.h),
                )
            }
            PaneDirection::Right => (
                rect.x >= active.x + active.w,
                rect.x - (active.x + active.w),
                interval_overlap(rect.y, rect.h, active.y, active.h),
            ),
            PaneDirection::Up => {
                let edge = rect.y + rect.h;
                (
                    edge <= active.y,
                    active.y - edge,
                    interval_overlap(rect.x, rect.w, active.x, active.w),
                )
            }
            PaneDirection::Down => (
                rect.y >= active.y + active.h,
                rect.y - (active.y + active.h),
                interval_overlap(rect.x, rect.w, active.x, active.w),
            ),
        };
        if !in_direction {
            continue;
        }
        if overlap > 0.0 {
            let better = match overlapping {
                None => true,
                Some((best_distance, best_overlap, best_id)) => {
                    distance < best_distance
                        || (distance == best_distance
                            && (overlap > best_overlap
                                || (overlap == best_overlap && id < best_id)))
                }
            };
            if better {
                overlapping = Some((distance, overlap, id));
            }
        }
        let center = center_distance_sq(rect, active);
        let better_center = match centered {
            None => true,
            Some((best, best_id)) => center < best || (center == best && id < best_id),
        };
        if better_center {
            centered = Some((center, id));
        }
    }
    overlapping
        .map(|(_, _, id)| id)
        .or_else(|| centered.map(|(_, id)| id))
}

/// Length of the shared part of two intervals; negative when they are apart.
#[inline]
fn interval_overlap(a_start: f32, a_len: f32, b_start: f32, b_len: f32) -> f32 {
    (a_start + a_len).min(b_start + b_len) - a_start.max(b_start)
}

#[inline]
fn center_distance_sq(a: Rect, b: Rect) -> f32 {
    let dx = (a.x + a.w * 0.5) - (b.x + b.w * 0.5);
    let dy = (a.y + a.h * 0.5) - (b.y + b.h * 0.5);
    dx * dx + dy * dy
}

/// Split node under a pointer press, addressed by its path from the root.
#[derive(Clone, Debug)]
pub(crate) struct DividerHit {
    pub path: Vec<u8>,
    pub axis: SplitAxis,
}

/// Divider under the point `(x, y)`. Every split node is laid out exactly as
/// [`layout_panes`] does; a node is hit when the point is inside its rect and
/// lies in the gap between its branches (or within the 3 logical px band
/// around the branch edge when the gap is zero). The nearest divider wins,
/// then the deeper node.
pub(crate) fn divider_at(
    root: &SplitNode,
    body: Rect,
    scale: f32,
    char_w: f32,
    char_h: f32,
    x: f32,
    y: f32,
) -> Option<DividerHit> {
    let mut probe = DividerProbe {
        scale,
        char_w,
        char_h,
        x,
        y,
        band: (3.0 * scale).round().max(1.0),
        path: Vec::new(),
        best: None,
    };
    probe.walk(root, body);
    probe.best.map(|(_, hit)| hit)
}

struct DividerProbe {
    scale: f32,
    char_w: f32,
    char_h: f32,
    x: f32,
    y: f32,
    band: f32,
    path: Vec<u8>,
    best: Option<(f32, DividerHit)>,
}

impl DividerProbe {
    fn walk(&mut self, node: &SplitNode, rect: Rect) {
        let SplitNode::Split {
            axis,
            ratio,
            first,
            second,
        } = node
        else {
            return;
        };
        if !rect.contains(self.x, self.y) {
            return;
        }
        let (first_rect, second_rect) = child_rects(
            rect,
            *axis,
            *ratio,
            first.as_ref(),
            second.as_ref(),
            self.scale,
            self.char_w,
            self.char_h,
        );
        let (coord, first_edge, second_start) = match axis {
            SplitAxis::SideBySide => (self.x, first_rect.x + first_rect.w, second_rect.x),
            SplitAxis::Stacked => (self.y, first_rect.y + first_rect.h, second_rect.y),
        };
        let gap = (second_start - first_edge).max(0.0);
        let hit = if gap > 0.0 {
            coord >= first_edge && coord <= second_start
        } else {
            (coord - first_edge).abs() <= self.band
        };
        if hit {
            // The divider line sits in the middle of the gap.
            let distance = (coord - (first_edge + gap * 0.5)).abs();
            let better = match &self.best {
                None => true,
                Some((best_distance, best_hit)) => {
                    distance < *best_distance
                        || (distance == *best_distance && self.path.len() > best_hit.path.len())
                }
            };
            if better {
                self.best = Some((
                    distance,
                    DividerHit {
                        path: self.path.clone(),
                        axis: *axis,
                    },
                ));
            }
        }
        self.path.push(0);
        self.walk(first.as_ref(), first_rect);
        self.path.pop();
        self.path.push(1);
        self.walk(second.as_ref(), second_rect);
        self.path.pop();
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

    /// At the clamp boundary `available * (min / available)` can land just
    /// below `min` in `f32`, so `floor` alone may drop a branch under its
    /// whole-pixel minimum.
    #[test]
    fn clamped_ratio_never_violates_minimum_at_f32_boundary() {
        let (scale, char_w, char_h) = (1.25_f32, 9.3_f32, 21.7_f32);
        let min_w = pane_min(char_w, char_h, scale).0.ceil();
        let tree = split(SplitAxis::SideBySide, 0.1, pane(1), pane(2));
        let body = Rect {
            x: 0.0,
            y: 0.0,
            w: 305.0,
            h: 400.0,
        };
        let mut out = Vec::new();
        layout_panes(&tree, body, scale, char_w, char_h, &mut out);
        let first = out[0].1;
        let second = out[1].1;
        assert!(first.w >= min_w, "first pane {first:?} below {min_w}");
        assert!(second.w >= min_w, "second pane {second:?} below {min_w}");
        assert_eq!(
            first.w + second.w + gap_for(scale, body.w),
            body.w,
            "panes plus gap must cover the node"
        );
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

    fn rect(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }

    /// Three panes in a pre-order line: `1`, then the nested `2`, `3`.
    fn line_of_three() -> SplitNode {
        split(
            SplitAxis::SideBySide,
            0.4,
            pane(1),
            split(SplitAxis::SideBySide, 0.5, pane(2), pane(3)),
        )
    }

    #[test]
    fn successor_prefers_previous_leaf_in_tree_order() {
        let line = line_of_three();
        assert_eq!(successor(&line, PaneId::new(2)), Some(PaneId::new(1)));
        assert_eq!(successor(&line, PaneId::new(1)), Some(PaneId::new(2)));
        assert_eq!(successor(&line, PaneId::new(3)), Some(PaneId::new(2)));
        // A single-leaf tree has no other pane to hand the focus to.
        assert_eq!(successor(&pane(7), PaneId::new(7)), None);
        // A pane that is not in the tree has no successor either.
        assert_eq!(successor(&line, PaneId::new(9)), None);
    }

    #[test]
    fn split_and_remove_keep_tree_shape() {
        let (char_w, char_h, scale) = (10.0_f32, 20.0_f32, 1.0_f32);
        let body = rect(0.0, 0.0, 1600.0, 1000.0);
        let mut tree = split(
            SplitAxis::SideBySide,
            0.5,
            split(SplitAxis::Stacked, 0.4, pane(1), pane(2)),
            split(SplitAxis::Stacked, 0.6, pane(3), pane(4)),
        );
        let mut before = Vec::new();
        layout_panes(&tree, body, scale, char_w, char_h, &mut before);
        assert_eq!(leaf_count(&tree), 4);
        assert_eq!(find_path(&tree, PaneId::new(4)), Some(vec![1, 1]));

        assert!(split_leaf_at(
            &mut tree,
            PaneId::new(4),
            SplitAxis::SideBySide,
            PaneId::new(5)
        ));
        assert_eq!(leaf_count(&tree), 5);
        assert_eq!(find_path(&tree, PaneId::new(5)), Some(vec![1, 1, 1]));
        assert_eq!(find_path(&tree, PaneId::new(4)), Some(vec![1, 1, 0]));
        let new_split = node_at_path(&tree, &[1, 1]).expect("split node sits at the leaf path");
        assert!(matches!(
            new_split,
            SplitNode::Split {
                axis: SplitAxis::SideBySide,
                ratio,
                ..
            } if *ratio == 0.5
        ));

        let mut split_out = Vec::new();
        layout_panes(&tree, body, scale, char_w, char_h, &mut split_out);
        assert_eq!(split_out[3].0, PaneId::new(4));
        assert_eq!(split_out[4].0, PaneId::new(5));
        assert_eq!(
            split_out[3].1.w + gap_for(scale, split_out[3].1.w + split_out[4].1.w) + split_out[4].1.w,
            before[3].1.w
        );

        assert!(remove_leaf(&mut tree, PaneId::new(5)));
        assert_eq!(leaf_count(&tree), 4);
        assert_eq!(find_path(&tree, PaneId::new(5)), None);
        assert!(matches!(
            node_at_path(&tree, &[1, 1]),
            Some(SplitNode::Leaf(id)) if *id == PaneId::new(4)
        ));
        let mut after = Vec::new();
        layout_panes(&tree, body, scale, char_w, char_h, &mut after);
        assert_eq!(after, before);
    }

    #[test]
    fn remove_leaf_never_removes_root_leaf() {
        let mut single = pane(1);
        assert!(!remove_leaf(&mut single, PaneId::new(1)));
        assert!(matches!(single, SplitNode::Leaf(id) if id == PaneId::new(1)));
        assert_eq!(leaf_count(&single), 1);

        let mut tree = split(SplitAxis::SideBySide, 0.5, pane(1), pane(2));
        assert!(!remove_leaf(&mut tree, PaneId::new(9)));
        assert_eq!(leaf_count(&tree), 2);
        assert_eq!(find_path(&tree, PaneId::new(1)), Some(vec![0]));
    }

    #[test]
    fn split_leaf_at_reports_missing_pane() {
        let mut tree = line_of_three();
        assert!(!split_leaf_at(
            &mut tree,
            PaneId::new(9),
            SplitAxis::Stacked,
            PaneId::new(4)
        ));
        assert_eq!(leaf_count(&tree), 3);
        assert_eq!(find_path(&tree, PaneId::new(4)), None);

        assert!(split_leaf_at(
            &mut tree,
            PaneId::new(1),
            SplitAxis::Stacked,
            PaneId::new(4)
        ));
        assert_eq!(leaf_count(&tree), 4);
        assert_eq!(find_path(&tree, PaneId::new(4)), Some(vec![0, 1]));
        assert_eq!(find_path(&tree, PaneId::new(1)), Some(vec![0, 0]));
    }

    #[test]
    fn pane_in_direction_prefers_overlap_then_distance() {
        let id = PaneId::new;
        // One pane on the left, a column of two on the right.
        let column = [
            (id(1), rect(0.0, 0.0, 494.0, 600.0)),
            (id(2), rect(500.0, 0.0, 494.0, 300.0)),
            (id(3), rect(500.0, 306.0, 494.0, 294.0)),
        ];
        assert_eq!(
            pane_in_direction(&column, id(2), PaneDirection::Left),
            Some(id(1))
        );
        assert_eq!(
            pane_in_direction(&column, id(3), PaneDirection::Left),
            Some(id(1))
        );
        assert_eq!(
            pane_in_direction(&column, id(2), PaneDirection::Down),
            Some(id(3))
        );
        assert_eq!(
            pane_in_direction(&column, id(3), PaneDirection::Up),
            Some(id(2))
        );
        // Both right panes are 6 px away: the taller overlap wins.
        assert_eq!(
            pane_in_direction(&column, id(1), PaneDirection::Right),
            Some(id(2))
        );
        assert_eq!(
            pane_in_direction(&column, id(1), PaneDirection::Left),
            None
        );
        assert_eq!(
            pane_in_direction(&column, id(3), PaneDirection::Right),
            None
        );

        // Pane 7 is closer along x but shares no y interval with pane 4, so
        // the overlapping pane 6 wins.
        let mixed = [
            (id(4), rect(500.0, 300.0, 400.0, 300.0)),
            (id(5), rect(500.0, 0.0, 400.0, 294.0)),
            (id(6), rect(906.0, 300.0, 94.0, 300.0)),
            (id(7), rect(900.0, 0.0, 100.0, 294.0)),
        ];
        assert_eq!(
            pane_in_direction(&mixed, id(4), PaneDirection::Right),
            Some(id(6))
        );
    }

    #[test]
    fn pane_in_direction_returns_none_at_edge_and_uses_center_distance_without_overlap() {
        let id = PaneId::new;
        let single = [(id(1), rect(0.0, 0.0, 100.0, 100.0))];
        for dir in [
            PaneDirection::Left,
            PaneDirection::Right,
            PaneDirection::Up,
            PaneDirection::Down,
        ] {
            assert_eq!(pane_in_direction(&single, id(1), dir), None);
            // The pane itself must be in the list of rects.
            assert_eq!(pane_in_direction(&single, id(9), dir), None);
        }

        // Pane 3 is closer along x, but its centre is farther away than the
        // centre of pane 2, so pane 2 wins.
        let far = [
            (id(1), rect(0.0, 0.0, 100.0, 100.0)),
            (id(2), rect(300.0, 300.0, 100.0, 100.0)),
            (id(3), rect(150.0, 500.0, 100.0, 100.0)),
        ];
        assert_eq!(
            pane_in_direction(&far, id(1), PaneDirection::Right),
            Some(id(2))
        );

        // Equal centre distance: the smaller pane id wins.
        let tie = [
            (id(1), rect(500.0, 500.0, 100.0, 100.0)),
            (id(6), rect(0.0, 0.0, 100.0, 100.0)),
            (id(5), rect(1000.0, 0.0, 100.0, 100.0)),
        ];
        assert_eq!(
            pane_in_direction(&tie, id(1), PaneDirection::Up),
            Some(id(5))
        );
    }

    #[test]
    fn divider_at_hits_gap_band_and_deepest_node() {
        let body = rect(0.0, 0.0, 1600.0, 1000.0);
        let (scale, char_w, char_h) = (1.0_f32, 10.0_f32, 20.0_f32);
        let pair = split(SplitAxis::SideBySide, 0.5, pane(1), pane(2));
        // 1600 px body, 6 px gap: the root gap spans x 797..803.
        let hit = divider_at(&pair, body, scale, char_w, char_h, 800.0, 500.0)
            .expect("the root gap is a divider");
        assert!(hit.path.is_empty(), "the root split sits at the empty path");
        assert_eq!(hit.axis, SplitAxis::SideBySide);
        assert!(divider_at(&pair, body, scale, char_w, char_h, 797.0, 500.0).is_some());
        assert!(divider_at(&pair, body, scale, char_w, char_h, 803.0, 500.0).is_some());
        // 10 px from the gap: outside the gap and outside the 3 px band.
        assert!(divider_at(&pair, body, scale, char_w, char_h, 790.0, 500.0).is_none());

        let nested = split(
            SplitAxis::SideBySide,
            0.5,
            pane(1),
            split(SplitAxis::Stacked, 0.5, pane(2), pane(3)),
        );
        // The nested horizontal gap spans y 497..503 inside the right branch.
        let nested_hit = divider_at(&nested, body, scale, char_w, char_h, 1000.0, 500.0)
            .expect("the nested gap is a divider");
        assert_eq!(nested_hit.path, vec![1]);
        assert_eq!(nested_hit.axis, SplitAxis::Stacked);
        // Same distance to both dividers: the deeper node wins.
        let deep = divider_at(&nested, body, scale, char_w, char_h, 803.0, 497.0)
            .expect("both dividers are hit");
        assert_eq!(deep.path, vec![1]);
        // A point in the root gap alone still resolves to the root.
        let root_only = divider_at(&nested, body, scale, char_w, char_h, 800.0, 900.0)
            .expect("the root gap is a divider");
        assert!(root_only.path.is_empty());
    }
}
