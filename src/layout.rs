//! Tiled layout of a tab: a tree of splits whose leaves are panes.

use std::collections::HashMap;

use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

pub use crate::workspace::Axis;

pub type PaneId = u32;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Node {
    Leaf(PaneId),
    /// Children with relative weights (any positive scale).
    Split {
        axis: Axis,
        children: Vec<(Node, f32)>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

/// A draggable boundary between two siblings, as laid out on screen.
#[derive(Debug, Clone, PartialEq)]
pub struct Border {
    /// Child indices from the root down to the split that owns this border.
    pub path: Vec<usize>,
    /// The border lies between child `index` and `index + 1`.
    pub index: usize,
    pub axis: Axis,
    /// Column (Row axis) or line (Column axis) of the first cell of child `index + 1`.
    pub pos: u16,
    /// Extent of the split along the axis, used to convert drags into weights.
    pub start: u16,
    pub len: u16,
    /// The cross-axis span covered by the border.
    pub span: (u16, u16),
}

impl Node {
    pub fn leaves(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { children, .. } => children.iter().for_each(|(c, _)| c.collect(out)),
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(p) => *p == id,
            Node::Split { children, .. } => children.iter().any(|(c, _)| c.contains(id)),
        }
    }

    /// Rectangles of every leaf inside `area`.
    pub fn layout(&self, area: Rect) -> HashMap<PaneId, Rect> {
        let mut out = HashMap::new();
        let mut borders = Vec::new();
        self.place(area, &mut Vec::new(), &mut out, &mut borders);
        out
    }

    /// Draggable borders inside `area`.
    pub fn borders(&self, area: Rect) -> Vec<Border> {
        let mut out = HashMap::new();
        let mut borders = Vec::new();
        self.place(area, &mut Vec::new(), &mut out, &mut borders);
        borders
    }

    fn place(&self, area: Rect, path: &mut Vec<usize>, out: &mut HashMap<PaneId, Rect>, borders: &mut Vec<Border>) {
        match self {
            Node::Leaf(id) => {
                out.insert(*id, area);
            }
            Node::Split { axis, children } => {
                let total = match axis {
                    Axis::Row => area.width,
                    Axis::Column => area.height,
                };
                let sizes = distribute(total, &children.iter().map(|(_, w)| *w).collect::<Vec<_>>());
                let mut offset = 0u16;
                for (i, ((child, _), size)) in children.iter().zip(&sizes).enumerate() {
                    let rect = match axis {
                        Axis::Row => Rect::new(area.x + offset, area.y, *size, area.height),
                        Axis::Column => Rect::new(area.x, area.y + offset, area.width, *size),
                    };
                    path.push(i);
                    child.place(rect, path, out, borders);
                    path.pop();
                    offset += size;
                    if i + 1 < children.len() {
                        let (pos, start, span) = match axis {
                            Axis::Row => (area.x + offset, area.x, (area.y, area.y + area.height)),
                            Axis::Column => (area.y + offset, area.y, (area.x, area.x + area.width)),
                        };
                        borders.push(Border {
                            path: path.clone(),
                            index: i,
                            axis: *axis,
                            pos,
                            start,
                            len: total,
                            span,
                        });
                    }
                }
            }
        }
    }

    fn at_path_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        let mut node = self;
        for &i in path {
            match node {
                Node::Split { children, .. } => node = &mut children.get_mut(i)?.0,
                Node::Leaf(_) => return None,
            }
        }
        Some(node)
    }

    /// Path of child indices to the leaf `id`.
    fn path_to(&self, id: PaneId) -> Option<Vec<usize>> {
        match self {
            Node::Leaf(p) => (*p == id).then(Vec::new),
            Node::Split { children, .. } => children.iter().enumerate().find_map(|(i, (c, _))| {
                let mut p = c.path_to(id)?;
                p.insert(0, i);
                Some(p)
            }),
        }
    }

    /// Insert `new` next to `target`, after it, along `axis`. The target's space is halved.
    pub fn split(&mut self, target: PaneId, axis: Axis, new: PaneId) -> bool {
        let Some(path) = self.path_to(target) else { return false };
        if let Some((&last, parent_path)) = path.split_last()
            && let Some(Node::Split { axis: a, children }) = self.at_path_mut(parent_path)
            && *a == axis
        {
            let w = children[last].1 / 2.0;
            children[last].1 = w;
            children.insert(last + 1, (Node::Leaf(new), w));
            return true;
        }
        let Some(slot) = self.at_path_mut(&path) else { return false };
        let old = std::mem::replace(slot, Node::Leaf(new));
        *slot = Node::Split { axis, children: vec![(old, 1.0), (Node::Leaf(new), 1.0)] };
        true
    }

    /// Remove a leaf. Returns `false` if it is the root leaf (the caller drops the tree).
    pub fn remove(&mut self, id: PaneId) -> bool {
        let Some(path) = self.path_to(id) else { return false };
        let Some((&last, parent_path)) = path.split_last() else { return false };
        if let Some(Node::Split { children, .. }) = self.at_path_mut(parent_path) {
            let (_, w) = children.remove(last);
            // Give the space to the neighbour that took its place (or the new last child).
            let n = last.min(children.len() - 1);
            children[n].1 += w;
        }
        self.normalize();
        true
    }

    /// Collapse single-child splits and splits nested along the same axis.
    pub fn normalize(&mut self) {
        if let Node::Split { axis, children } = self {
            for (c, _) in children.iter_mut() {
                c.normalize();
            }
            let mut flat = Vec::new();
            for (c, w) in children.drain(..) {
                match c {
                    Node::Split { axis: a, children: inner } if a == *axis => {
                        let sum: f32 = inner.iter().map(|(_, iw)| iw).sum::<f32>().max(f32::EPSILON);
                        flat.extend(inner.into_iter().map(|(ic, iw)| (ic, w * iw / sum)));
                    }
                    c => flat.push((c, w)),
                }
            }
            *children = flat;
            if children.len() == 1 {
                let (only, _) = children.pop().unwrap();
                *self = only;
            }
        }
    }

    /// Grow or shrink pane `id` by `cells` towards `dir`, moving the nearest border on that side.
    pub fn resize(&mut self, id: PaneId, dir: Dir, cells: i32, area: Rect) -> bool {
        let rects = self.layout(area);
        let Some(rect) = rects.get(&id).copied() else { return false };
        let borders = self.borders(area);
        // The border on the requested side of the pane.
        let (axis, edge) = match dir {
            Dir::Left => (Axis::Row, rect.x),
            Dir::Right => (Axis::Row, rect.x + rect.width),
            Dir::Up => (Axis::Column, rect.y),
            Dir::Down => (Axis::Column, rect.y + rect.height),
        };
        let cross = match axis {
            Axis::Row => (rect.y, rect.y + rect.height),
            Axis::Column => (rect.x, rect.x + rect.width),
        };
        let Some(border) = borders
            .iter()
            .filter(|b| b.axis == axis && b.pos == edge && b.span.0 <= cross.0 && b.span.1 >= cross.1)
            .min_by_key(|b| b.span.1 - b.span.0)
        else {
            return false;
        };
        let delta = match dir {
            Dir::Left | Dir::Up => -cells,
            Dir::Right | Dir::Down => cells,
        };
        let new_pos = (border.pos as i32 + delta).clamp(0, u16::MAX as i32) as u16;
        self.drag_border(border, new_pos)
    }

    /// Move a border (from [`Node::borders`]) so that it sits at `pos`.
    pub fn drag_border(&mut self, border: &Border, pos: u16) -> bool {
        let Some(Node::Split { children, .. }) = self.at_path_mut(&border.path) else { return false };
        let i = border.index;
        if i + 1 >= children.len() {
            return false;
        }
        let weights: Vec<f32> = children.iter().map(|(_, w)| *w).collect();
        let sizes = distribute(border.len, &weights);
        let before: u16 = sizes[..i].iter().sum();
        let pair = sizes[i] + sizes[i + 1];
        let min = 3u16.min(pair / 2);
        let first = (pos.saturating_sub(border.start).saturating_sub(before)).clamp(min, pair - min);
        if first == sizes[i] {
            return false;
        }
        let total_w: f32 = weights.iter().sum();
        let per_cell = total_w / border.len.max(1) as f32;
        children[i].1 = first as f32 * per_cell;
        children[i + 1].1 = (pair - first) as f32 * per_cell;
        true
    }
}

/// Split `total` cells among weights, rounding so the sizes add up exactly.
pub fn distribute(total: u16, weights: &[f32]) -> Vec<u16> {
    if weights.is_empty() {
        return Vec::new();
    }
    let sum: f32 = weights.iter().map(|w| w.max(0.0)).sum();
    let mut sizes = Vec::with_capacity(weights.len());
    let mut acc = 0.0f32;
    let mut used = 0u16;
    for w in weights {
        acc += if sum > 0.0 { w.max(0.0) / sum } else { 1.0 / weights.len() as f32 };
        let end = ((acc * total as f32).round() as u16).min(total);
        sizes.push(end.saturating_sub(used));
        used = end;
    }
    if let Some(last) = sizes.last_mut() {
        *last += total - used;
    }
    sizes
}

/// The pane in direction `dir` from `from`, preferring the largest overlap, then `recent`.
pub fn neighbour(rects: &HashMap<PaneId, Rect>, from: PaneId, dir: Dir, recent: &[PaneId]) -> Option<PaneId> {
    let r = rects.get(&from)?;
    let overlap = |a0: u16, a1: u16, b0: u16, b1: u16| a1.min(b1).saturating_sub(a0.max(b0));
    // Closest first, then most recently focused, then largest overlap.
    type Rank = (u16, usize, std::cmp::Reverse<u16>);
    let mut best: Option<(PaneId, Rank)> = None;
    for (&id, o) in rects {
        if id == from {
            continue;
        }
        let (gap, ov) = match dir {
            Dir::Left if o.x + o.width <= r.x => {
                (r.x - (o.x + o.width), overlap(r.y, r.y + r.height, o.y, o.y + o.height))
            }
            Dir::Right if o.x >= r.x + r.width => {
                (o.x - (r.x + r.width), overlap(r.y, r.y + r.height, o.y, o.y + o.height))
            }
            Dir::Up if o.y + o.height <= r.y => {
                (r.y - (o.y + o.height), overlap(r.x, r.x + r.width, o.x, o.x + o.width))
            }
            Dir::Down if o.y >= r.y + r.height => {
                (o.y - (r.y + r.height), overlap(r.x, r.x + r.width, o.x, o.x + o.width))
            }
            _ => continue,
        };
        if ov == 0 {
            continue;
        }
        let rank = recent.iter().position(|p| *p == id).unwrap_or(usize::MAX);
        let key = (gap, rank, std::cmp::Reverse(ov));
        if best.as_ref().is_none_or(|(_, k)| key < *k) {
            best = Some((id, key));
        }
    }
    best.map(|(id, _)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    #[test]
    fn distribute_sums_exactly() {
        assert_eq!(distribute(10, &[1.0, 1.0, 1.0]), vec![3, 4, 3]);
        assert_eq!(distribute(7, &[1.0]), vec![7]);
        assert_eq!(distribute(0, &[1.0, 2.0]), vec![0, 0]);
        let s = distribute(101, &[0.3, 0.3, 0.4]);
        assert_eq!(s.iter().sum::<u16>(), 101);
    }

    #[test]
    fn split_same_axis_appends_sibling() {
        let mut n = Node::Leaf(1);
        n.split(1, Axis::Row, 2);
        n.split(2, Axis::Row, 3);
        let Node::Split { children, .. } = &n else { panic!() };
        assert_eq!(children.len(), 3);
        let r = n.layout(area());
        assert_eq!(r[&1].width, 50);
        assert_eq!(r[&2].width + r[&3].width, 50);
    }

    #[test]
    fn split_cross_axis_nests() {
        let mut n = Node::Leaf(1);
        n.split(1, Axis::Row, 2);
        n.split(2, Axis::Column, 3);
        let r = n.layout(area());
        assert_eq!(r[&2].x, 50);
        assert_eq!(r[&3].x, 50);
        assert_eq!(r[&3].y, 20);
        assert_eq!(r[&1].height, 40);
    }

    #[test]
    fn remove_collapses() {
        let mut n = Node::Leaf(1);
        n.split(1, Axis::Row, 2);
        n.split(2, Axis::Column, 3);
        assert!(n.remove(3));
        assert_eq!(n.leaves(), vec![1, 2]);
        assert!(n.remove(2));
        assert_eq!(n, Node::Leaf(1));
        assert!(!n.remove(1));
    }

    #[test]
    fn neighbours() {
        let mut n = Node::Leaf(1);
        n.split(1, Axis::Row, 2);
        n.split(2, Axis::Column, 3);
        let r = n.layout(area());
        assert_eq!(neighbour(&r, 1, Dir::Right, &[3]), Some(3));
        assert_eq!(neighbour(&r, 1, Dir::Right, &[2]), Some(2));
        assert_eq!(neighbour(&r, 3, Dir::Up, &[]), Some(2));
        assert_eq!(neighbour(&r, 3, Dir::Left, &[]), Some(1));
        assert_eq!(neighbour(&r, 1, Dir::Left, &[]), None);
    }

    #[test]
    fn resize_moves_border() {
        let mut n = Node::Leaf(1);
        n.split(1, Axis::Row, 2);
        assert!(n.resize(1, Dir::Right, 10, area()));
        let r = n.layout(area());
        assert_eq!(r[&1].width, 60);
        assert!(n.resize(2, Dir::Left, 5, area()));
        let r = n.layout(area());
        assert_eq!(r[&1].width, 55);
        assert!(!n.resize(1, Dir::Left, 5, area()));
    }

    #[test]
    fn drag_border_clamps() {
        let mut n = Node::Leaf(1);
        n.split(1, Axis::Column, 2);
        let b = n.borders(area())[0].clone();
        assert_eq!(b.pos, 20);
        n.drag_border(&b, 0);
        let r = n.layout(area());
        assert_eq!(r[&1].height, 3);
    }

    #[test]
    fn normalize_flattens_same_axis() {
        let mut n = Node::Split {
            axis: Axis::Row,
            children: vec![
                (Node::Leaf(1), 1.0),
                (Node::Split { axis: Axis::Row, children: vec![(Node::Leaf(2), 1.0), (Node::Leaf(3), 1.0)] }, 1.0),
            ],
        };
        n.normalize();
        let Node::Split { children, .. } = &n else { panic!() };
        assert_eq!(children.len(), 3);
        assert!((children[1].1 - 0.5).abs() < 1e-6);
    }
}
