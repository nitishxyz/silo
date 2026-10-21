//! Pure operations on a window's split layout, shared by the app (which
//! applies them locally at once) and the server (which runs them for
//! `layout.*`, `block.focus_*` and `window.step`).

use crate::{
    control::Direction,
    model::{Axis, BlockId, Layout},
};

/// Smallest share either side of a split keeps (as `Layout::set_ratio`).
pub const MIN_RATIO: f32 = 0.05;
/// How far one resize step moves a divider.
pub const RESIZE_STEP: f32 = 0.05;

/// A split's new ratio, by its path from the root (0 = first child).
pub type RatioChange = (Vec<usize>, f32);

/// The index `delta` places from `current` among `len`, wrapping.
#[must_use]
pub fn step(len: usize, current: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let next = (current as i64 + i64::from(delta)).rem_euclid(len as i64) as usize;
    next
}

/// The path from the root to leaf `id`.
#[must_use]
pub fn leaf_path(layout: &Layout, id: &str) -> Option<Vec<usize>> {
    match layout {
        Layout::Block { id: b } => (b == id).then(Vec::new),
        Layout::Unknown => None,
        Layout::Split { first, second, .. } => [first, second].into_iter().enumerate().find_map(|(i, child)| {
            let mut path = leaf_path(child, id)?;
            path.insert(0, i);
            Some(path)
        }),
    }
}

fn node_at<'a>(layout: &'a Layout, path: &[usize]) -> Option<&'a Layout> {
    match (layout, path.split_first()) {
        (_, None) => Some(layout),
        (Layout::Split { first, .. }, Some((0, rest))) => node_at(first, rest),
        (Layout::Split { second, .. }, Some((_, rest))) => node_at(second, rest),
        (Layout::Block { .. } | Layout::Unknown, Some(_)) => None,
    }
}

/// The axis and ratio of the split at `path`.
#[must_use]
pub fn split_ratio(layout: &Layout, path: &[usize]) -> Option<(Axis, f32)> {
    match node_at(layout, path)? {
        Layout::Split { axis, ratio, .. } => Some((*axis, *ratio)),
        Layout::Block { .. } | Layout::Unknown => None,
    }
}

/// The split at `path` going from `old` to `new` (clamped), if that
/// changes it.
#[must_use]
pub fn change(path: &[usize], old: f32, new: f32) -> Option<RatioChange> {
    let new = new.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
    ((new - old).abs() > 1e-4).then(|| (path.to_vec(), new))
}

/// Same tree of splits and blocks, ratios aside.
#[must_use]
pub fn same_shape(a: &Layout, b: &Layout) -> bool {
    match (a, b) {
        (Layout::Block { id: x }, Layout::Block { id: y }) => x == y,
        (Layout::Unknown, Layout::Unknown) => true,
        (Layout::Split { axis: ax, first: a1, second: a2, .. }, Layout::Split { axis: bx, first: b1, second: b2, .. }) => {
            ax == bx && same_shape(a1, b1) && same_shape(a2, b2)
        }
        _ => false,
    }
}

/// Grow (`delta > 0`) or shrink pane `id`'s share of its own split.
#[must_use]
pub fn resize_share(layout: &Layout, id: &str, delta: f32) -> Option<RatioChange> {
    let path = leaf_path(layout, id)?;
    let (&side, parent) = path.split_last()?;
    let (_, ratio) = split_ratio(layout, parent)?;
    change(parent, ratio, if side == 0 { ratio + delta } else { ratio - delta })
}

/// Move pane `id`'s edge on the `direction` side one step that way: the
/// nearest enclosing split along that axis whose divider is that edge.
#[must_use]
pub fn move_edge(layout: &Layout, id: &str, direction: Direction) -> Option<RatioChange> {
    let (axis, side, delta) = match direction {
        Direction::Left => (Axis::Horizontal, 1, -RESIZE_STEP),
        Direction::Right => (Axis::Horizontal, 0, RESIZE_STEP),
        Direction::Up => (Axis::Vertical, 1, -RESIZE_STEP),
        Direction::Down => (Axis::Vertical, 0, RESIZE_STEP),
    };
    let path = leaf_path(layout, id)?;
    let depth = (0..path.len()).rev().find(|&d| path[d] == side && split_ratio(layout, &path[..d]).is_some_and(|(a, _)| a == axis))?;
    let (_, ratio) = split_ratio(layout, &path[..depth])?;
    change(&path[..depth], ratio, ratio + delta)
}

/// Panes a subtree lines up along `axis`.
fn weight(layout: &Layout, axis: Axis) -> u32 {
    match layout {
        Layout::Split { axis: a, first, second, .. } if *a == axis => weight(first, axis) + weight(second, axis),
        _ => 1,
    }
}

/// The ratio giving every pane along the split at `path` an even share.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn even_split(layout: &Layout, path: &[usize]) -> Option<RatioChange> {
    let Layout::Split { axis, ratio, first, second } = node_at(layout, path)? else { return None };
    let (a, b) = (weight(first, *axis), weight(second, *axis));
    change(path, *ratio, a as f32 / (a + b) as f32)
}

/// Every split of `layout` evened out.
#[must_use]
pub fn equalize(layout: &Layout) -> Vec<RatioChange> {
    fn walk(root: &Layout, node: &Layout, path: &mut Vec<usize>, out: &mut Vec<RatioChange>) {
        if let Layout::Split { first, second, .. } = node {
            out.extend(even_split(root, path));
            for (i, child) in [first, second].into_iter().enumerate() {
                path.push(i);
                walk(root, child, path, out);
                path.pop();
            }
        }
    }
    let mut out = Vec::new();
    walk(layout, layout, &mut Vec::new(), &mut out);
    out
}

/// Axis-aligned rectangle: `(left, top, right, bottom)`.
pub type Rect = (f32, f32, f32, f32);

/// Each block's rectangle when `layout` fills `area`, splits leaving
/// `divider` between their halves.
#[must_use]
pub fn rects(layout: &Layout, area: Rect, divider: f32) -> Vec<(BlockId, Rect)> {
    fn walk(layout: &Layout, (l, t, r, b): Rect, divider: f32, out: &mut Vec<(BlockId, Rect)>) {
        match layout {
            Layout::Block { id } => out.push((id.clone(), (l, t, r, b))),
            Layout::Unknown => {}
            Layout::Split { axis, ratio, first, second } => {
                let ratio = ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO);
                match axis {
                    Axis::Horizontal => {
                        let x = l + (r - l - divider) * ratio;
                        walk(first, (l, t, x, b), divider, out);
                        walk(second, (x + divider, t, r, b), divider, out);
                    }
                    Axis::Vertical => {
                        let y = t + (b - t - divider) * ratio;
                        walk(first, (l, t, r, y), divider, out);
                        walk(second, (l, y + divider, r, b), divider, out);
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(layout, area, divider, &mut out);
    out
}

/// Index of the pane to focus from `current` in `direction`, or `None` if
/// nothing lies that way. Candidates must start past the current pane's
/// edge (within a few pixels for the dividers). Panes that overlap it on
/// the perpendicular axis win over ones that don't; then the nearest edge,
/// then the largest overlap, then the closest perpendicular centre.
#[must_use]
pub fn neighbour(current: Rect, candidates: &[Rect], direction: Direction) -> Option<usize> {
    const SLACK: f32 = 4.0;
    let (cl, ct, cr, cb) = current;
    let mut best: Option<(usize, (bool, f32, f32, f32))> = None;
    for (index, &(l, t, r, b)) in candidates.iter().enumerate() {
        let (gap, overlap, offset) = match direction {
            Direction::Left if r <= cl + SLACK => (cl - r, span_overlap(ct, cb, t, b), ((t + b) - (ct + cb)).abs() / 2.0),
            Direction::Right if l >= cr - SLACK => (l - cr, span_overlap(ct, cb, t, b), ((t + b) - (ct + cb)).abs() / 2.0),
            Direction::Up if b <= ct + SLACK => (ct - b, span_overlap(cl, cr, l, r), ((l + r) - (cl + cr)).abs() / 2.0),
            Direction::Down if t >= cb - SLACK => (t - cb, span_overlap(cl, cr, l, r), ((l + r) - (cl + cr)).abs() / 2.0),
            _ => continue,
        };
        // Smaller is better on every component.
        let key = (overlap <= 0.0, gap.max(0.0), -overlap, offset);
        let better =
            best.as_ref().is_none_or(|(_, k)| (key.0, key.1, key.2, key.3).partial_cmp(&(k.0, k.1, k.2, k.3)).is_some_and(std::cmp::Ordering::is_lt));
        if better {
            best = Some((index, key));
        }
    }
    best.map(|(index, _)| index)
}

fn span_overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> f32 {
    a1.min(b1) - a0.max(b0)
}

/// The tiled block next to `id` in `direction`, the layout filling a
/// square with hairline dividers (the server has no pixels).
#[must_use]
pub fn neighbour_in(layout: &Layout, id: &str, direction: Direction) -> Option<BlockId> {
    let mut all = rects(layout, (0.0, 0.0, 1000.0, 1000.0), 1.0);
    let index = all.iter().position(|(b, _)| b == id)?;
    let (_, from) = all.remove(index);
    let others: Vec<Rect> = all.iter().map(|(_, r)| *r).collect();
    neighbour(from, &others, direction).map(|i| all.swap_remove(i).0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Direction::*;

    fn block(id: &str) -> Layout {
        Layout::Block { id: id.into() }
    }

    fn split(axis: Axis, first: Layout, second: Layout) -> Layout {
        Layout::Split { axis, ratio: 0.5, first: Box::new(first), second: Box::new(second) }
    }

    fn ratio(axis: Axis, ratio: f32, first: Layout, second: Layout) -> Layout {
        Layout::Split { axis, ratio, first: Box::new(first), second: Box::new(second) }
    }

    fn close(change: Option<RatioChange>, path: &[usize], expected: f32) {
        let (p, r) = change.expect("a change");
        assert_eq!(p, path);
        assert!((r - expected).abs() < 1e-5, "{r} != {expected}");
    }

    #[test]
    fn steps_wrap_both_ways() {
        assert_eq!(step(3, 0, 1), 1);
        assert_eq!(step(3, 2, 1), 0);
        assert_eq!(step(3, 0, -1), 2);
        assert_eq!(step(3, 1, -4), 0);
        assert_eq!(step(0, 0, 1), 0);
    }

    #[test]
    fn grow_and_shrink_change_the_focused_share_of_its_split() {
        // a | (b / c)
        let layout = split(Axis::Horizontal, block("a"), split(Axis::Vertical, block("b"), block("c")));
        close(resize_share(&layout, "a", RESIZE_STEP), &[], 0.55);
        close(resize_share(&layout, "a", -RESIZE_STEP), &[], 0.45);
        close(resize_share(&layout, "b", RESIZE_STEP), &[1], 0.55);
        close(resize_share(&layout, "c", RESIZE_STEP), &[1], 0.45);
        assert_eq!(resize_share(&block("a"), "a", RESIZE_STEP), None);
        // Clamped to the minimum pane size; no change at the limit.
        let tight = ratio(Axis::Horizontal, 0.92, block("a"), block("b"));
        close(resize_share(&tight, "a", RESIZE_STEP), &[], 0.95);
        let full = ratio(Axis::Horizontal, 0.95, block("a"), block("b"));
        assert_eq!(resize_share(&full, "a", RESIZE_STEP), None);
        close(resize_share(&full, "b", RESIZE_STEP), &[], 0.9);
    }

    #[test]
    fn edges_move_on_the_nearest_split_bounding_that_side() {
        // (a | b) | c, with b split vertically into b / d.
        let layout = split(Axis::Horizontal, split(Axis::Horizontal, block("a"), split(Axis::Vertical, block("b"), block("d"))), block("c"));
        close(move_edge(&layout, "b", Right), &[], 0.55);
        close(move_edge(&layout, "b", Left), &[0], 0.45);
        close(move_edge(&layout, "b", Down), &[0, 1], 0.55);
        assert_eq!(move_edge(&layout, "b", Up), None);
        close(move_edge(&layout, "d", Up), &[0, 1], 0.45);
        assert_eq!(move_edge(&layout, "a", Left), None);
        close(move_edge(&layout, "a", Right), &[0], 0.55);
        close(move_edge(&layout, "c", Left), &[], 0.45);
        assert_eq!(move_edge(&layout, "c", Right), None);
        let edge = ratio(Axis::Horizontal, 0.05, block("a"), block("b"));
        assert_eq!(move_edge(&edge, "b", Left), None, "clamped");
    }

    #[test]
    fn equalize_gives_every_pane_along_an_axis_an_even_share() {
        // a | (b | c) | ... with an unrelated vertical split in c.
        let layout =
            ratio(Axis::Horizontal, 0.7, block("a"), ratio(Axis::Horizontal, 0.2, block("b"), ratio(Axis::Vertical, 0.9, block("c"), block("d"))));
        let changes = equalize(&layout);
        assert_eq!(changes.len(), 3);
        close(changes.first().cloned(), &[], 1.0 / 3.0);
        close(changes.get(1).cloned(), &[1], 0.5);
        close(changes.get(2).cloned(), &[1, 1], 0.5);
        let mut even = layout.clone();
        for (path, r) in &changes {
            even.set_ratio(path, *r);
        }
        assert!(equalize(&even).is_empty());
        close(even_split(&layout, &[1, 1]), &[1, 1], 0.5);
        assert_eq!(even_split(&layout, &[0]), None);
    }

    #[test]
    fn shape_ignores_ratios_but_not_splits() {
        let a = ratio(Axis::Horizontal, 0.3, block("a"), block("b"));
        assert!(same_shape(&a, &ratio(Axis::Horizontal, 0.6, block("a"), block("b"))));
        assert!(!same_shape(&a, &ratio(Axis::Vertical, 0.3, block("a"), block("b"))));
        assert!(!same_shape(&a, &ratio(Axis::Horizontal, 0.3, block("a"), ratio(Axis::Vertical, 0.5, block("b"), block("c")))));
        assert!(!same_shape(&a, &block("a")));
    }

    /// 2x2 grid of 100x100 panes with 1px dividers:
    /// 0 1
    /// 2 3
    const GRID: [Rect; 4] = [(0.0, 0.0, 100.0, 100.0), (101.0, 0.0, 201.0, 100.0), (0.0, 101.0, 100.0, 201.0), (101.0, 101.0, 201.0, 201.0)];

    #[test]
    fn two_by_two_grid() {
        let at = |i: usize, d| {
            let others: Vec<usize> = (0..4).filter(|j| *j != i).collect();
            let rects: Vec<_> = others.iter().map(|j| GRID[*j]).collect();
            neighbour(GRID[i], &rects, d).map(|k| others[k])
        };
        assert_eq!(at(0, Right), Some(1));
        assert_eq!(at(0, Down), Some(2));
        assert_eq!(at(0, Left), None);
        assert_eq!(at(0, Up), None);
        assert_eq!(at(3, Left), Some(2));
        assert_eq!(at(3, Up), Some(1));
        assert_eq!(at(1, Down), Some(3));
        assert_eq!(at(2, Right), Some(3));
    }

    #[test]
    fn prefers_overlapping_neighbour_over_nearer_diagonal() {
        // Left column is one tall pane; right column split in two.
        let current = (101.0, 101.0, 201.0, 201.0); // bottom right
        let candidates = [(101.0, 0.0, 201.0, 100.0), (0.0, 0.0, 100.0, 201.0)];
        assert_eq!(neighbour(current, &candidates, Left), Some(1));
        assert_eq!(neighbour(current, &candidates, Up), Some(0));
        // From the tall left pane, Right picks the pane sharing the most
        // edge with it.
        let tall = (0.0, 0.0, 100.0, 201.0);
        let right = [(101.0, 0.0, 201.0, 60.0), (101.0, 61.0, 201.0, 201.0)];
        assert_eq!(neighbour(tall, &right, Right), Some(1));
    }

    #[test]
    fn layouts_lay_out_and_neighbours_follow_them() {
        // (a / c) | (b / d): the 2x2 grid as a layout.
        let grid = split(Axis::Horizontal, split(Axis::Vertical, block("a"), block("c")), split(Axis::Vertical, block("b"), block("d")));
        let laid = rects(&grid, (0.0, 0.0, 201.0, 201.0), 1.0);
        let at = |id: &str| laid.iter().find(|(b, _)| b == id).map(|(_, r)| *r).unwrap();
        assert_eq!(at("a"), GRID[0]);
        assert_eq!(at("b"), GRID[1]);
        assert_eq!(at("c"), GRID[2]);
        assert_eq!(at("d"), GRID[3]);
        let next = |id: &str, d| neighbour_in(&grid, id, d);
        assert_eq!(next("a", Right).as_deref(), Some("b"));
        assert_eq!(next("a", Down).as_deref(), Some("c"));
        assert_eq!(next("d", Left).as_deref(), Some("c"));
        assert_eq!(next("d", Up).as_deref(), Some("b"));
        assert_eq!(next("a", Left), None);
        // A tall left pane and a right column split unevenly: right of the
        // tall pane is the lower, larger one.
        let uneven = split(Axis::Horizontal, block("tall"), ratio(Axis::Vertical, 0.3, block("top"), block("bottom")));
        assert_eq!(neighbour_in(&uneven, "tall", Right).as_deref(), Some("bottom"));
        assert_eq!(neighbour_in(&uneven, "top", Left).as_deref(), Some("tall"));
        assert_eq!(neighbour_in(&uneven, "ghost", Left), None);
    }
}
