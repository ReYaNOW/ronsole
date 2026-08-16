#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TabDragState {
    pub start_idx: usize,
    pub start_x: f32,
    pub current_x: f32,
    pub threshold_passed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TabDragPlacement {
    pub dragged_x: f32,
    pub destination: usize,
}

pub(crate) fn tab_drag_placement(
    base_x: f32,
    widths: &[f32],
    drag: Option<&TabDragState>,
) -> Option<TabDragPlacement> {
    let drag = drag?;
    if !drag.threshold_passed || drag.start_idx >= widths.len() {
        return None;
    }

    let start_x = base_x + widths[..drag.start_idx].iter().sum::<f32>();
    let dragged_x = start_x + (drag.current_x - drag.start_x);
    let dragged_center = dragged_x + widths[drag.start_idx] * 0.5;
    let mut destination = drag.start_idx;
    let mut x = base_x;

    for (idx, &width) in widths.iter().enumerate() {
        if idx != drag.start_idx {
            let other_center = x + width * 0.5;
            if idx < drag.start_idx {
                if dragged_center < other_center {
                    destination = destination.min(idx);
                }
            } else if dragged_center > other_center {
                destination = destination.max(idx);
            }
        }
        x += width;
    }

    Some(TabDragPlacement {
        dragged_x,
        destination,
    })
}

pub(crate) fn tab_drag_layout(
    base_x: f32,
    widths: &[f32],
    drag: Option<&TabDragState>,
    actual_xs: &mut Vec<f32>,
    order: &mut Vec<usize>,
) -> Option<usize> {
    actual_xs.clear();
    order.clear();
    actual_xs.reserve(widths.len());
    order.reserve(widths.len());

    let mut x = base_x;
    for (idx, &width) in widths.iter().enumerate() {
        actual_xs.push(x);
        order.push(idx);
        x += width;
    }

    let drag = drag?;
    let placement = tab_drag_placement(base_x, widths, Some(drag))?;
    order.retain(|&idx| idx != drag.start_idx);
    order.insert(placement.destination, drag.start_idx);

    let mut x = base_x;
    for &idx in order.iter() {
        if idx != drag.start_idx {
            actual_xs[idx] = x;
        }
        x += widths[idx];
    }
    actual_xs[drag.start_idx] = placement.dragged_x;
    Some(drag.start_idx)
}

pub(crate) fn tab_drag_render_order(
    order: &[usize],
    dragged_idx: Option<usize>,
    render_order: &mut Vec<usize>,
) {
    render_order.clear();
    render_order.reserve(order.len());
    render_order.extend(
        order
            .iter()
            .copied()
            .filter(|idx| Some(*idx) != dragged_idx),
    );
    if let Some(idx) = dragged_idx {
        render_order.push(idx);
    }
}

pub(crate) fn active_index_after_move(active: usize, from: usize, to: usize) -> usize {
    if active == from {
        to
    } else if from < to && active > from && active <= to {
        active - 1
    } else if to < from && active >= to && active < from {
        active + 1
    } else {
        active
    }
}

pub(crate) fn active_index_after_remove(active: usize, removed: usize, remaining: usize) -> usize {
    if remaining == 0 {
        return 0;
    }
    if active == removed {
        removed.min(remaining - 1)
    } else if active > removed {
        active - 1
    } else {
        active.min(remaining - 1)
    }
}

pub(crate) fn take_terminal_creation_number(next: &mut u64) -> u64 {
    let number = *next;
    *next = number.saturating_add(1);
    number
}

pub(crate) fn update_tab_x_animation(
    animated_xs: &mut Vec<f32>,
    actual_xs: &[f32],
    dragged_idx: Option<usize>,
) -> bool {
    if animated_xs.len() != actual_xs.len() || dragged_idx.is_none() {
        let changed = animated_xs.as_slice() != actual_xs;
        animated_xs.clear();
        animated_xs.extend_from_slice(actual_xs);
        return changed;
    }

    let mut active = false;
    for (idx, &target_x) in actual_xs.iter().enumerate() {
        if Some(idx) == dragged_idx {
            if (animated_xs[idx] - target_x).abs() > f32::EPSILON {
                active = true;
            }
            animated_xs[idx] = target_x;
            continue;
        }
        let diff = target_x - animated_xs[idx];
        if diff.abs() > 0.5 {
            animated_xs[idx] += diff * 0.12;
            active = true;
        } else {
            animated_xs[idx] = target_x;
        }
    }
    active
}

pub(crate) fn tab_strip_reveal_target(
    widths: &[f32],
    active_idx: usize,
    viewport_w: f32,
    current_target: f32,
    margin: f32,
) -> f32 {
    if active_idx >= widths.len() || viewport_w <= 0.0 {
        return 0.0;
    }

    let tab_left = widths[..active_idx].iter().sum::<f32>();
    let tab_right = tab_left + widths[active_idx];
    let total_w = widths.iter().sum::<f32>();
    let max_scroll = (total_w - viewport_w).max(0.0);
    let margin = margin.max(0.0).min(viewport_w * 0.25);
    let mut target = current_target;

    if tab_left < target + margin {
        target = tab_left - margin;
    } else if tab_right > target + viewport_w - margin {
        target = tab_right + margin - viewport_w;
    }

    target.clamp(0.0, max_scroll)
}

pub(crate) const DRAG_AUTOSCROLL_EDGE_PX: f32 = 58.0;

pub(crate) fn drag_autoscroll_delta(pos: f32, start: f32, end: f32, edge: f32) -> f32 {
    if pos < start {
        pos - start
    } else if pos < start + edge {
        pos - start - edge
    } else if pos > end {
        pos - end
    } else if pos > end - edge {
        pos - end + edge
    } else {
        0.0
    }
}

pub(crate) fn drag_autoscroll_speed(delta: f32) -> f32 {
    crate::scroll::drag_autoscroll_speed(delta, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_identity_survives_moves_and_removals() {
        assert_eq!(active_index_after_move(0, 0, 2), 2);
        assert_eq!(active_index_after_move(1, 0, 2), 0);
        assert_eq!(active_index_after_move(1, 2, 0), 2);
        assert_eq!(active_index_after_move(2, 1, 0), 2);
        assert_eq!(active_index_after_remove(2, 0, 2), 1);
        assert_eq!(active_index_after_remove(1, 1, 2), 1);
        assert_eq!(active_index_after_remove(0, 0, 0), 0);
    }

    #[test]
    fn creation_numbers_are_monotonic_and_not_reused() {
        let mut next = 1;
        assert_eq!(take_terminal_creation_number(&mut next), 1);
        assert_eq!(take_terminal_creation_number(&mut next), 2);
        assert_eq!(take_terminal_creation_number(&mut next), 3);
        assert_eq!(take_terminal_creation_number(&mut next), 4);
        assert_eq!(next, 5);
    }

    #[test]
    fn drag_uses_centers_and_renders_dragged_tab_last() {
        let widths = [100.0, 120.0, 80.0];
        let drag = TabDragState {
            start_idx: 0,
            start_x: 50.0,
            current_x: 270.0,
            threshold_passed: true,
        };
        let placement = tab_drag_placement(0.0, &widths, Some(&drag)).unwrap();
        assert_eq!(placement.destination, 2);

        let mut xs = Vec::new();
        let mut order = Vec::new();
        let dragged = tab_drag_layout(0.0, &widths, Some(&drag), &mut xs, &mut order);
        assert_eq!(dragged, Some(0));
        assert_eq!(order, vec![1, 2, 0]);

        let mut render_order = Vec::new();
        tab_drag_render_order(&order, dragged, &mut render_order);
        assert_eq!(render_order.last().copied(), Some(0));
    }

    #[test]
    fn one_tab_drag_keeps_identity_and_position() {
        let widths = [120.0];
        let drag = TabDragState {
            start_idx: 0,
            start_x: 40.0,
            current_x: 70.0,
            threshold_passed: true,
        };
        let placement = tab_drag_placement(10.0, &widths, Some(&drag)).unwrap();
        assert_eq!(placement.destination, 0);
        assert_eq!(placement.dragged_x, 40.0);
    }

    #[test]
    fn reveal_target_handles_tail_old_tabs_fractional_scale_and_narrow_viewports() {
        let widths = [133.25, 91.5, 177.75, 44.0];
        let viewport = 220.0;
        let tail = tab_strip_reveal_target(&widths, 3, viewport, 0.0, 8.0 * 1.3333334);
        assert!(tail > 0.0);
        let max = widths.iter().sum::<f32>() - viewport;
        assert!(tail <= max);
        let old = tab_strip_reveal_target(&widths, 0, viewport, tail, 10.0);
        assert_eq!(old, 0.0);
        assert_eq!(tab_strip_reveal_target(&[80.0], 0, 10.0, 500.0, 8.0), 0.0);
    }

    #[test]
    fn drag_autoscroll_matches_donor_edge_math_and_bounds_speed() {
        assert_eq!(drag_autoscroll_delta(50.0, 100.0, 500.0, 40.0), -50.0);
        assert_eq!(drag_autoscroll_delta(120.0, 100.0, 500.0, 40.0), -20.0);
        assert_eq!(drag_autoscroll_delta(480.0, 100.0, 500.0, 40.0), 20.0);
        assert_eq!(drag_autoscroll_delta(540.0, 100.0, 500.0, 40.0), 40.0);
        assert_eq!(drag_autoscroll_delta(250.0, 100.0, 500.0, 40.0), 0.0);
        assert!(drag_autoscroll_speed(30.0) >= crate::scroll::DRAG_AUTOSCROLL_MIN_SPEED);
        assert!(drag_autoscroll_speed(10_000.0) <= crate::scroll::DRAG_AUTOSCROLL_MAX_SPEED);
    }

    #[test]
    fn neighbor_animation_moves_smoothly_only_during_drag() {
        let mut animated = vec![0.0, 100.0, 200.0];
        let actual = vec![0.0, 180.0, 100.0];
        assert!(update_tab_x_animation(&mut animated, &actual, Some(0)));
        assert!(animated[1] > 100.0 && animated[1] < 180.0);
        assert!(animated[2] < 200.0 && animated[2] > 100.0);
        assert!(update_tab_x_animation(&mut animated, &actual, None));
        assert_eq!(animated, actual);
        assert!(!update_tab_x_animation(&mut animated, &actual, None));
    }
}
