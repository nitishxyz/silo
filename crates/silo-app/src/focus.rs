//! Directional pane focus (⌘H/J/K/L): pick the geometric neighbour of the
//! focused pane in the split layout.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Axis-aligned rectangle: `(left, top, right, bottom)`.
pub type Rect = (f32, f32, f32, f32);

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

#[cfg(test)]
mod tests {
    use super::{Direction::*, neighbour};

    /// 2x2 grid of 100x100 panes with 1px dividers:
    /// 0 1
    /// 2 3
    const GRID: [(f32, f32, f32, f32); 4] =
        [(0.0, 0.0, 100.0, 100.0), (101.0, 0.0, 201.0, 100.0), (0.0, 101.0, 100.0, 201.0), (101.0, 101.0, 201.0, 201.0)];

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
}
