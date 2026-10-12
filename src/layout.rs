//! Tiled layout of a tab: a tree of splits whose leaves are panes, or stacks of panes sharing a
//! place.

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
    /// Panes sharing one place, top to bottom: `open` shows whole, the others only as the top
    /// line of their frame.
    Stack {
        panes: Vec<PaneId>,
        open: PaneId,
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

impl Border {
    /// Whether a press at (`col`, `row`) takes this border: with frames, on the frames either
    /// side of it and on the column of space between tiles side by side ([`framed`]); without,
    /// on the first column or row after it.
    pub fn grabbed_at(&self, col: u16, row: u16, frames: bool) -> bool {
        let (along, across) = match self.axis {
            Axis::Row => (col, row),
            Axis::Column => (row, col),
        };
        let space = u16::from(self.axis == Axis::Row);
        let from = if frames { self.pos.saturating_sub(1 + space) } else { self.pos };
        (from..=self.pos).contains(&along) && (self.span.0..self.span.1).contains(&across)
    }
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
            Node::Stack { panes, .. } => out.extend(panes),
        }
    }

    pub fn contains(&self, id: PaneId) -> bool {
        match self {
            Node::Leaf(p) => *p == id,
            Node::Split { children, .. } => children.iter().any(|(c, _)| c.contains(id)),
            Node::Stack { panes, .. } => panes.contains(&id),
        }
    }

    /// Each place of the tree in order, as the panes there and the one shown: a pane alone, or
    /// a stack and its open pane.
    pub fn places(&self) -> Vec<(&[PaneId], PaneId)> {
        let mut out = Vec::new();
        self.collect_places(&mut out);
        out
    }

    fn collect_places<'a>(&'a self, out: &mut Vec<(&'a [PaneId], PaneId)>) {
        match self {
            Node::Leaf(id) => out.push((std::slice::from_ref(id), *id)),
            Node::Split { children, .. } => children.iter().for_each(|(c, _)| c.collect_places(out)),
            Node::Stack { panes, open } => out.push((panes, *open)),
        }
    }

    /// The stack holding pane `id`: its panes, and the one open.
    pub fn stack_of(&self, id: PaneId) -> Option<(&[PaneId], PaneId)> {
        self.places().into_iter().find(|(panes, _)| panes.len() > 1 && panes.contains(&id))
    }

    /// The pane shown in the place of `id`: itself, or the open pane of its stack.
    pub fn holder(&self, id: PaneId) -> PaneId {
        self.stack_of(id).map_or(id, |(_, open)| open)
    }

    /// The pane shown in the first place: the pane there, or the open pane of a stack there.
    pub fn first_shown(&self) -> Option<PaneId> {
        self.places().first().map(|&(_, open)| open)
    }

    /// Open pane `id` in its stack, folding the one open before. Whether anything changed.
    pub fn expand(&mut self, id: PaneId) -> bool {
        match self {
            Node::Leaf(_) => false,
            Node::Split { children, .. } => children.iter_mut().any(|(c, _)| c.expand(id)),
            Node::Stack { panes, open } if panes.contains(&id) && *open != id => {
                *open = id;
                true
            }
            Node::Stack { .. } => false,
        }
    }

    /// Rectangles of every place inside `area`, by the pane shown there: each pane, and for a
    /// stack the pane open in it (the others share its place).
    pub fn layout(&self, area: Rect) -> HashMap<PaneId, Rect> {
        let mut out = HashMap::new();
        let mut borders = Vec::new();
        self.place(area, &mut Vec::new(), &mut out, &mut borders);
        out
    }

    /// Every pane in `area`, in order: the frame of each pane shown whole, and the row of each
    /// pane folded in a stack (marked `true`; one without room has none). With `gaps`, a frame
    /// leaves a column of space before the places right of it ([`framed`]).
    pub fn tiles(&self, area: Rect, gaps: bool) -> Vec<(PaneId, Rect, bool)> {
        let places = self.layout(area);
        let mut out = Vec::new();
        for (panes, open) in self.places() {
            let Some(&place) = places.get(&open) else { continue };
            let (frame, rows) = fold(if gaps { framed(place, area) } else { place }, panes, open);
            out.push((open, frame, false));
            out.extend(rows.into_iter().map(|(p, r)| (p, r, true)));
        }
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
            Node::Leaf(id) | Node::Stack { open: id, .. } => {
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
                Node::Leaf(_) | Node::Stack { .. } => return None,
            }
        }
        Some(node)
    }

    /// Path of child indices to the place of `id`: its leaf, or its stack.
    fn path_to(&self, id: PaneId) -> Option<Vec<usize>> {
        match self {
            Node::Leaf(p) => (*p == id).then(Vec::new),
            Node::Stack { panes, .. } => panes.contains(&id).then(Vec::new),
            Node::Split { children, .. } => children.iter().enumerate().find_map(|(i, (c, _))| {
                let mut p = c.path_to(id)?;
                p.insert(0, i);
                Some(p)
            }),
        }
    }

    /// Insert `new` next to `target`, after it, along `axis`. The target's space is halved; a
    /// stacked target's whole stack shares it.
    pub fn split(&mut self, target: PaneId, axis: Axis, new: PaneId) -> bool {
        self.insert(target, axis, new, false)
    }

    fn insert(&mut self, target: PaneId, axis: Axis, new: PaneId, before: bool) -> bool {
        let Some(path) = self.path_to(target) else { return false };
        if let Some((&last, parent_path)) = path.split_last()
            && let Some(Node::Split { axis: a, children }) = self.at_path_mut(parent_path)
            && *a == axis
        {
            let w = children[last].1 / 2.0;
            children[last].1 = w;
            children.insert(if before { last } else { last + 1 }, (Node::Leaf(new), w));
            return true;
        }
        let Some(slot) = self.at_path_mut(&path) else { return false };
        let old = std::mem::replace(slot, Node::Leaf(new));
        let pair = if before { [(Node::Leaf(new), 1.0), (old, 1.0)] } else { [(old, 1.0), (Node::Leaf(new), 1.0)] };
        *slot = Node::Split { axis, children: pair.into() };
        true
    }

    /// Put `new` in the stack holding `target`, right after it, and open it.
    pub fn join(&mut self, target: PaneId, new: PaneId) -> bool {
        let Some(path) = self.path_to(target) else { return false };
        match self.at_path_mut(&path) {
            Some(Node::Stack { panes, open }) => {
                let i = panes.iter().position(|p| *p == target).map_or(panes.len(), |i| i + 1);
                panes.insert(i, new);
                *open = new;
                true
            }
            _ => false,
        }
    }

    /// Move pane `id` into the place of `onto`, after the panes there or before them: that
    /// place becomes a stack if it held one pane, and `id` is open in it.
    pub fn stack(&mut self, id: PaneId, onto: PaneId, after: bool) -> bool {
        if id == onto || !self.contains(onto) || self.stack_of(id).is_some_and(|(p, _)| p.contains(&onto)) {
            return false;
        }
        if !self.remove(id) {
            return false;
        }
        let Some(path) = self.path_to(onto) else { return false };
        let Some(slot) = self.at_path_mut(&path) else { return false };
        match slot {
            Node::Leaf(o) => {
                let o = *o;
                *slot = Node::Stack { panes: if after { vec![o, id] } else { vec![id, o] }, open: id };
            }
            Node::Stack { panes, open } => {
                if after {
                    panes.push(id);
                } else {
                    panes.insert(0, id);
                }
                *open = id;
            }
            Node::Split { .. } => return false,
        }
        true
    }

    /// Take pane `id` out of its stack, into a place of its own below the stack (above it when
    /// it was the stack's first pane).
    pub fn unstack(&mut self, id: PaneId) -> bool {
        let Some((panes, _)) = self.stack_of(id) else { return false };
        let first = panes[0] == id;
        let Some(&other) = panes.iter().find(|p| **p != id) else { return false };
        self.remove(id) && self.insert(other, Axis::Column, id, first)
    }

    /// Remove a leaf. Returns `false` if it is the root leaf (the caller drops the tree).
    pub fn remove(&mut self, id: PaneId) -> bool {
        let Some(path) = self.path_to(id) else { return false };
        // A pane leaves its stack, which keeps its place; the next pane opens if it was open.
        if let Some(Node::Stack { panes, open }) = self.at_path_mut(&path) {
            let Some(i) = panes.iter().position(|p| *p == id) else { return false };
            panes.remove(i);
            if *open == id
                && let Some(next) = panes.get(i).or(panes.last())
            {
                *open = *next;
            }
            self.normalize();
            return true;
        }
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

    /// Collapse single-child splits, splits nested along the same axis, and stacks of one pane.
    pub fn normalize(&mut self) {
        match self {
            Node::Leaf(_) => {}
            Node::Stack { panes, open } => {
                if let [only] = panes[..] {
                    *self = Node::Leaf(only);
                } else if !panes.contains(open)
                    && let Some(first) = panes.first()
                {
                    *open = *first;
                }
            }
            Node::Split { axis, children } => {
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
    }

    /// Grow or shrink pane `id` by `cells` towards `dir`, moving the nearest border on that side.
    pub fn resize(&mut self, id: PaneId, dir: Dir, cells: i32, area: Rect) -> bool {
        let rects = self.layout(area);
        let Some(rect) = rects.get(&self.holder(id)).copied() else { return false };
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

/// Where the frame of a place inside `area` goes: the place, less one column of space when a
/// place is to its right. Rows of places touch, frame on frame: a cell is about twice as tall as
/// it is wide, so a row of space would look twice as wide as that column.
fn framed(place: Rect, area: Rect) -> Rect {
    let right = u16::from(place.right() < area.right() && place.width > 1);
    Rect::new(place.x, place.y, place.width - right, place.height)
}

/// Rows a stack's open pane keeps before the folded ones get theirs: a frame and a line.
const OPEN_MIN: u16 = 3;

/// How a stack fills `area`, the frame of its place: the open pane's frame, and the row of each
/// folded pane, those before it above and those after it below. When rows run short, the panes
/// farthest from the open one go without.
pub fn fold(area: Rect, panes: &[PaneId], open: PaneId) -> (Rect, Vec<(PaneId, Rect)>) {
    let i = panes.iter().position(|p| *p == open).unwrap_or(0);
    let (mut above, mut below) = (i, panes.len().saturating_sub(i + 1));
    let room = usize::from(area.height.saturating_sub(OPEN_MIN));
    while above + below > room {
        if above >= below {
            above -= 1;
        } else {
            below -= 1;
        }
    }
    let frame = Rect::new(area.x, area.y + above as u16, area.width, area.height - (above + below) as u16);
    let row = |y: u16| Rect::new(area.x, y, area.width, 1);
    let mut rows = Vec::with_capacity(above + below);
    for (k, p) in panes[i - above..i].iter().enumerate() {
        rows.push((*p, row(area.y + k as u16)));
    }
    for (k, p) in panes[i + 1..i + 1 + below].iter().enumerate() {
        rows.push((*p, row(frame.bottom() + k as u16)));
    }
    (frame, rows)
}

/// Panes side by side along `axis`, as many as there are.
fn line(axis: Axis, ids: &[PaneId]) -> Node {
    match ids {
        [one] => Node::Leaf(*one),
        _ => Node::Split { axis, children: ids.iter().map(|id| (Node::Leaf(*id), 1.0)).collect() },
    }
}

/// Tile panes (at least one) in a grid of places wider than tall, like terminal cells, its rows
/// as even as they can be (the first ones take a pane more).
pub fn grid(ids: &[PaneId]) -> Node {
    let n = ids.len();
    let cols = ((n as f64 * 1.5).sqrt().ceil() as usize).clamp(1, n.max(1));
    let count = n.div_ceil(cols);
    let mut rows = Vec::with_capacity(count);
    let mut rest = ids;
    for r in 0..count {
        let (row, more) = rest.split_at(rest.len().div_ceil(count - r));
        rows.push((line(Axis::Row, row), 1.0));
        rest = more;
    }
    match rows.len() {
        1 => rows.remove(0).0,
        _ => Node::Split { axis: Axis::Column, children: rows },
    }
}

/// Ready-made arrangements of a tab's tiled panes, in the order a key goes through them. The
/// first pane is the main one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    Columns,
    Rows,
    MainLeft,
    MainLeftStacked,
    MainTop,
    Grid,
}

impl Preset {
    pub const ALL: [Preset; 6] =
        [Preset::Columns, Preset::Rows, Preset::MainLeft, Preset::MainLeftStacked, Preset::MainTop, Preset::Grid];

    pub fn describe(self) -> &'static str {
        match self {
            Preset::Columns => "even columns",
            Preset::Rows => "even rows",
            Preset::MainLeft => "main pane left",
            Preset::MainLeftStacked => "main pane left, the others stacked",
            Preset::MainTop => "main pane on top",
            Preset::Grid => "grid",
        }
    }

    /// `panes` in this arrangement, in their order; a stack opens `focus` when it holds it.
    pub fn build(self, panes: &[PaneId], focus: Option<PaneId>) -> Option<Node> {
        // The main pane takes three fifths.
        let main = |axis: Axis, first: PaneId, rest: Node| Node::Split {
            axis,
            children: vec![(Node::Leaf(first), 3.0), (rest, 2.0)],
        };
        let (&first, rest) = panes.split_first()?;
        if rest.is_empty() {
            return Some(Node::Leaf(first));
        }
        Some(match self {
            Preset::Columns => line(Axis::Row, panes),
            Preset::Rows => line(Axis::Column, panes),
            Preset::MainLeft => main(Axis::Row, first, line(Axis::Column, rest)),
            Preset::MainLeftStacked => {
                let open = focus.filter(|f| rest.contains(f)).unwrap_or(rest[0]);
                let mut stack = Node::Stack { panes: rest.to_vec(), open };
                stack.normalize();
                main(Axis::Row, first, stack)
            }
            Preset::MainTop => main(Axis::Column, first, line(Axis::Row, rest)),
            Preset::Grid => grid(panes),
        })
    }

    /// The arrangement after `current` (the first one without) that changes `root`, skipping
    /// those that would look like one before them, with the tree it gives.
    pub fn next(current: Option<Preset>, root: &Node, focus: Option<PaneId>) -> Option<(Preset, Node)> {
        let panes = root.leaves();
        let built: Vec<(Preset, Node)> =
            Preset::ALL.iter().filter_map(|p| Some((*p, p.build(&panes, focus)?))).collect();
        let start = current.and_then(|c| built.iter().position(|(p, _)| *p == c)).map_or(0, |i| i + 1);
        (0..built.len()).map(|k| (start + k) % built.len()).find_map(|i| {
            let (_, tree) = &built[i];
            let first_of_its_kind = built.iter().position(|(_, t)| t == tree) == Some(i);
            (first_of_its_kind && tree != root).then(|| built[i].clone())
        })
    }
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

    fn stack(panes: &[PaneId], open: PaneId) -> Node {
        Node::Stack { panes: panes.to_vec(), open }
    }

    #[test]
    fn a_stack_takes_one_place() {
        let mut n = Node::Split { axis: Axis::Row, children: vec![(Node::Leaf(1), 1.0), (stack(&[2, 3, 4], 3), 1.0)] };
        let r = n.layout(area());
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(r[&3], Rect::new(50, 0, 50, 40));
        assert_eq!(n.leaves(), vec![1, 2, 3, 4]);
        assert_eq!((n.holder(2), n.holder(1)), (3, 1));
        assert_eq!(n.first_shown(), Some(1));
        let stack_first =
            Node::Split { axis: Axis::Row, children: vec![(stack(&[2, 3, 4], 3), 1.0), (Node::Leaf(1), 1.0)] };
        assert_eq!(stack_first.first_shown(), Some(3), "the open pane, not the first");
        assert_eq!(n.stack_of(4), Some((&[2, 3, 4][..], 3)));
        assert_eq!(n.stack_of(1), None);
        assert_eq!(n.borders(area()).len(), 1);
        // The folded panes take a row each, those before the open one above it.
        assert_eq!(
            n.tiles(area(), true),
            vec![
                (1, Rect::new(0, 0, 49, 40), false),
                (3, Rect::new(50, 1, 50, 38), false),
                (2, Rect::new(50, 0, 50, 1), true),
                (4, Rect::new(50, 39, 50, 1), true),
            ]
        );
        // Moving into a stack from beside it reaches its open pane.
        assert_eq!(neighbour(&r, 1, Dir::Right, &[2, 4]), Some(3));
        assert_eq!(neighbour(&r, 3, Dir::Left, &[]), Some(1));
        assert!(n.expand(4) && !n.expand(4) && !n.expand(1));
        assert_eq!(n.holder(2), 4);
        // Resizing a folded pane resizes its stack.
        assert!(n.resize(2, Dir::Left, 10, area()));
        assert_eq!(n.layout(area())[&4].x, 40);
    }

    #[test]
    fn tiles_keep_a_column_between_them_and_their_rows_touch() {
        let mut n = Node::Leaf(1);
        n.split(1, Axis::Row, 2);
        n.split(2, Axis::Column, 3);
        let want = vec![
            (1, Rect::new(0, 0, 49, 40), false),
            (2, Rect::new(50, 0, 50, 20), false),
            (3, Rect::new(50, 20, 50, 20), false),
        ];
        assert_eq!(n.tiles(area(), true), want);
        assert_eq!(n.tiles(area(), false)[0], (1, Rect::new(0, 0, 50, 40), false), "without frames, no space");
    }

    #[test]
    fn a_border_is_grabbed_on_the_frames_either_side_of_it() {
        let mut n = Node::Leaf(1);
        n.split(1, Axis::Row, 2);
        n.split(2, Axis::Column, 3);
        let borders = n.borders(area());
        let grabbed = |col, row, frames| {
            borders.iter().filter(|b| b.grabbed_at(col, row, frames)).map(|b| b.axis).collect::<Vec<_>>()
        };
        // Between the columns: the left frame, the space, the right frame.
        for col in [48, 49, 50] {
            assert_eq!(grabbed(col, 10, true), [Axis::Row], "{col}");
        }
        assert!(grabbed(47, 10, true).is_empty() && grabbed(51, 10, true).is_empty());
        // Between the rows, which touch: the bottom of the upper frame, the top of the lower one.
        for row in [19, 20] {
            assert_eq!(grabbed(70, row, true), [Axis::Column], "{row}");
        }
        assert!(grabbed(70, 18, true).is_empty() && grabbed(70, 21, true).is_empty());
        assert!(grabbed(30, 20, true).is_empty(), "not beside the split");
        // Without frames, the first column or row after the split.
        assert_eq!((grabbed(50, 10, false), grabbed(70, 20, false)), (vec![Axis::Row], vec![Axis::Column]));
        assert!(grabbed(49, 10, false).is_empty() && grabbed(70, 19, false).is_empty());
    }

    #[test]
    fn a_stack_folds_its_panes_inside_its_place() {
        let mut n = Node::Split { axis: Axis::Column, children: vec![(stack(&[1, 2], 2), 1.0), (Node::Leaf(3), 1.0)] };
        let tiles = n.tiles(area(), true);
        assert_eq!(tiles[0], (2, Rect::new(0, 1, 100, 19), false));
        assert_eq!(tiles[1], (1, Rect::new(0, 0, 100, 1), true));
        assert_eq!(tiles[2], (3, Rect::new(0, 20, 100, 20), false));
        n.expand(1);
        let tiles = n.tiles(area(), true);
        assert_eq!(tiles[0], (1, Rect::new(0, 0, 100, 19), false));
        assert_eq!(tiles[1], (2, Rect::new(0, 19, 100, 1), true), "under the frame, right above the next row");
        assert_eq!(n.tiles(area(), false), tiles, "rows touch, with frames or not");
    }

    #[test]
    fn folding_keeps_the_rows_nearest_the_open_pane() {
        let ids = [1, 2, 3, 4, 5];
        let rows = |r: Vec<(PaneId, Rect)>| r.into_iter().map(|(p, r)| (p, r.y)).collect::<Vec<_>>();
        let (frame, r) = fold(Rect::new(0, 0, 30, 10), &ids, 3);
        assert_eq!((frame, rows(r)), (Rect::new(0, 2, 30, 6), vec![(1, 0), (2, 1), (4, 8), (5, 9)]));
        let (frame, r) = fold(Rect::new(0, 0, 30, 5), &ids, 3);
        assert_eq!((frame, rows(r)), (Rect::new(0, 1, 30, 3), vec![(2, 0), (4, 4)]));
        let (frame, r) = fold(Rect::new(0, 0, 30, 10), &ids, 1);
        assert_eq!((frame, rows(r)), (Rect::new(0, 0, 30, 6), vec![(2, 6), (3, 7), (4, 8), (5, 9)]));
        let (frame, r) = fold(Rect::new(0, 0, 30, 6), &ids, 5);
        assert_eq!((frame, rows(r)), (Rect::new(0, 3, 30, 3), vec![(2, 0), (3, 1), (4, 2)]));
        // Too small for any row: the open pane has it all.
        for h in [0, 1, 2, 3] {
            let (frame, r) = fold(Rect::new(4, 2, 30, h), &ids, 3);
            assert_eq!((frame, r.len()), (Rect::new(4, 2, 30, h), 0));
        }
        let (frame, r) = fold(Rect::new(0, 0, 0, 0), &[1, 2], 2);
        assert!(frame.is_empty() && r.is_empty());
    }

    #[test]
    fn stacking_moves_a_pane_into_a_place_and_back_out() {
        let mut n = Node::Leaf(1);
        n.split(1, Axis::Row, 2);
        n.split(2, Axis::Column, 3);
        // 1 goes right, onto 2: before it, open.
        assert!(n.stack(1, 2, false));
        let one_two =
            Node::Split { axis: Axis::Column, children: vec![(stack(&[1, 2], 1), 1.0), (Node::Leaf(3), 1.0)] };
        assert_eq!(n, one_two);
        assert!(!n.stack(1, 2, true), "already stacked together");
        assert!(!n.stack(1, 9, true));
        // 3 goes up, onto the stack: after its panes.
        assert!(n.stack(3, 1, true));
        assert_eq!(n, stack(&[1, 2, 3], 3));
        // Out again: below the stack, the next pane open.
        assert!(n.unstack(3));
        assert_eq!(
            n,
            Node::Split { axis: Axis::Column, children: vec![(stack(&[1, 2], 2), 1.0), (Node::Leaf(3), 1.0)] }
        );
        // The first pane goes above, and a stack of one is a pane.
        assert!(n.unstack(1));
        assert_eq!(n.leaves(), vec![1, 2, 3]);
        assert!(n.stack_of(2).is_none());
        assert!(matches!(&n, Node::Split { axis: Axis::Column, children } if children.len() == 3));
        assert!(!n.unstack(2));
        // Splitting a stacked pane splits its stack's place; a new pane may join the stack.
        let mut n = stack(&[1, 2], 1);
        assert!(n.join(1, 3));
        assert_eq!(n, stack(&[1, 3, 2], 3));
        assert!(n.split(2, Axis::Row, 4));
        assert_eq!(
            n,
            Node::Split { axis: Axis::Row, children: vec![(stack(&[1, 3, 2], 3), 1.0), (Node::Leaf(4), 1.0)] }
        );
        assert!(!n.join(4, 5), "4 is not stacked");
    }

    #[test]
    fn closing_a_stacked_pane_opens_the_next_one() {
        let mut n = Node::Split { axis: Axis::Row, children: vec![(Node::Leaf(9), 1.0), (stack(&[1, 2, 3], 2), 1.0)] };
        assert!(n.remove(2));
        assert_eq!(n.stack_of(1), Some((&[1, 3][..], 3)));
        assert!(n.remove(3));
        assert_eq!(n, Node::Split { axis: Axis::Row, children: vec![(Node::Leaf(9), 1.0), (Node::Leaf(1), 1.0)] });
        let mut n = stack(&[1, 2], 2);
        assert!(n.remove(2));
        assert_eq!(n, Node::Leaf(1));
    }

    #[test]
    fn presets_keep_the_order_of_the_panes() {
        let panes = [1, 2, 3, 4];
        let at = |p: Preset| p.build(&panes, Some(3)).unwrap().layout(area());
        let r = at(Preset::Columns);
        let cols = panes.map(|p| (r[&p].x, r[&p].width, r[&p].height));
        assert_eq!(cols, [(0, 25, 40), (25, 25, 40), (50, 25, 40), (75, 25, 40)]);
        let r = at(Preset::Rows);
        let rows = panes.map(|p| (r[&p].y, r[&p].height, r[&p].width));
        assert_eq!(rows, [(0, 10, 100), (10, 10, 100), (20, 10, 100), (30, 10, 100)]);
        let r = at(Preset::MainLeft);
        assert_eq!(r[&1], Rect::new(0, 0, 60, 40));
        assert_eq!([2, 3, 4].map(|p| (r[&p].x, r[&p].width)), [(60, 40); 3]);
        assert_eq!([2, 3, 4].map(|p| r[&p].height).iter().sum::<u16>(), 40);
        // The others stacked: the focused one open.
        let tree = Preset::MainLeftStacked.build(&panes, Some(3)).unwrap();
        assert_eq!(tree.stack_of(2), Some((&[2, 3, 4][..], 3)));
        assert_eq!(tree.layout(area())[&3], Rect::new(60, 0, 40, 40));
        let tree = Preset::MainLeftStacked.build(&panes, Some(1)).unwrap();
        assert_eq!(tree.holder(4), 2);
        let r = at(Preset::MainTop);
        assert_eq!(r[&1], Rect::new(0, 0, 100, 24));
        assert_eq!([2, 3, 4].map(|p| (r[&p].y, r[&p].height)), [(24, 16); 3]);
        let r = at(Preset::Grid);
        let cells = [(0, 0), (50, 0), (0, 20), (50, 20)].map(|(x, y)| Rect::new(x, y, 50, 20));
        assert_eq!(panes.map(|p| r[&p]), cells);
        for p in Preset::ALL {
            assert_eq!(p.build(&panes, None).unwrap().leaves(), panes, "{p:?}");
            assert_eq!(p.build(&[7], None), Some(Node::Leaf(7)));
            assert_eq!(p.build(&[], None), None);
        }
    }

    #[test]
    fn presets_fit_any_size() {
        let sizes = [(100, 40), (20, 6), (7, 3), (3, 2), (1, 1), (0, 0)];
        for p in Preset::ALL {
            for n in 1..=7u32 {
                let panes: Vec<PaneId> = (1..=n).collect();
                let tree = p.build(&panes, Some(n)).unwrap();
                for (w, h) in sizes {
                    let area = Rect::new(2, 1, w, h);
                    let tiles = tree.tiles(area, true);
                    let shown: Vec<PaneId> = tiles.iter().map(|(p, ..)| *p).collect();
                    assert!(shown.iter().all(|p| panes.contains(p)), "{p:?} {n} {w}x{h}");
                    assert_eq!(tiles.iter().filter(|(.., folded)| !folded).count(), tree.places().len());
                    for (i, (a, ra, _)) in tiles.iter().enumerate() {
                        assert_eq!(ra.intersection(area), *ra, "{p:?} {n} {w}x{h}: pane {a} at {ra:?} outside");
                        for (b, rb, _) in &tiles[i + 1..] {
                            assert!(!ra.intersects(*rb), "{p:?} {n} {w}x{h}: {a} {ra:?} and {b} {rb:?} overlap");
                        }
                    }
                    if (w, h) == (100, 40) {
                        assert_eq!(shown.len(), panes.len(), "{p:?} {n}: every pane has a place or a row");
                    }
                }
            }
        }
    }

    #[test]
    fn the_layout_key_goes_through_presets_that_look_different() {
        let mut root = Preset::Columns.build(&[1, 2], None).unwrap();
        let mut current = None;
        let mut seen = Vec::new();
        for _ in 0..4 {
            let (p, tree) = Preset::next(current, &root, Some(1)).unwrap();
            assert_ne!(tree, root);
            seen.push(p);
            (current, root) = (Some(p), tree);
        }
        // With two panes, the stacked one is the main pane left, and the grid is two columns.
        assert_eq!(seen, [Preset::Rows, Preset::MainLeft, Preset::MainTop, Preset::Columns]);
        let three = Preset::Rows.build(&[1, 2, 3], None).unwrap();
        let (p, tree) = Preset::next(Some(Preset::MainLeft), &three, Some(3)).unwrap();
        assert_eq!(p, Preset::MainLeftStacked);
        assert_eq!(tree.stack_of(2), Some((&[2, 3][..], 3)));
        assert_eq!(Preset::next(None, &Node::Leaf(1), Some(1)), None);
    }

    #[test]
    fn grids() {
        assert_eq!(grid(&[1]), Node::Leaf(1));
        let g = grid(&[1, 2, 3, 4]);
        let Node::Split { axis: Axis::Column, children } = &g else { panic!("{g:?}") };
        assert_eq!(children.len(), 2);
        assert!(children.iter().all(|(row, _)| row.leaves().len() == 2), "{g:?}");
        let g = grid(&(1..=9).collect::<Vec<_>>());
        assert_eq!(g.leaves(), (1..=9).collect::<Vec<_>>());
        let Node::Split { children, .. } = &g else { panic!() };
        assert_eq!(children.len(), 3);
        let g = grid(&[1, 2]);
        assert!(matches!(g, Node::Split { axis: Axis::Row, .. }));
        let g = grid(&[1, 2, 3, 4, 5]);
        let Node::Split { children, .. } = &g else { panic!() };
        assert_eq!(children.iter().map(|(row, _)| row.leaves().len()).collect::<Vec<_>>(), [3, 2]);
    }

    /// Layouts as kraal builds them (any tree, any weights a file or a drag gives), laid out in any
    /// area and changed any way: no panic, the places inside the area.
    #[test]
    fn fuzz_layouts() {
        fn tree(rng: &mut crate::fuzz::Rng, depth: u32, next: &mut PaneId) -> Node {
            let leaf = |next: &mut PaneId| {
                *next += 1;
                *next
            };
            match rng.below(if depth > 3 { 2 } else { 4 }) {
                0 => Node::Leaf(leaf(next)),
                1 => {
                    // A stack holds two panes at least: kraal makes none of fewer.
                    let panes: Vec<PaneId> = (0..rng.range(2, 5)).map(|_| leaf(next)).collect();
                    let open = if rng.one_in(4) { 9999 } else { panes[0] };
                    Node::Stack { panes, open }
                }
                _ => Node::Split {
                    axis: if rng.one_in(2) { Axis::Row } else { Axis::Column },
                    children: (0..rng.size(5))
                        .map(|_| {
                            let w = *rng.pick(&[1.0, 0.0, -1.0, 1e-30, 1e30, f32::MAX, 0.5]);
                            (tree(rng, depth + 1, next), w)
                        })
                        .collect(),
                },
            }
        }
        crate::fuzz::run("layouts", 500, |rng| {
            let mut next = 0;
            let mut root = tree(rng, 0, &mut next);
            // As kraal makes every tree (a session file's too): no split of one child.
            root.normalize();
            let area = |rng: &mut crate::fuzz::Rng| {
                let (w, h) = (rng.below(300) as u16, rng.below(120) as u16);
                Rect::new(rng.below(10) as u16, rng.below(10) as u16, w, h)
            };
            for _ in 0..rng.range(1, 20) {
                let a = area(rng);
                let ids = root.leaves();
                let id = if ids.is_empty() { 1 } else { *rng.pick(&ids) };
                match rng.below(12) {
                    0 => {
                        next += 1;
                        root.split(id, if rng.one_in(2) { Axis::Row } else { Axis::Column }, next);
                    }
                    1 => {
                        next += 1;
                        root.join(id, next);
                    }
                    2 => {
                        let onto = if ids.is_empty() { 1 } else { *rng.pick(&ids) };
                        root.stack(id, onto, rng.one_in(2));
                    }
                    3 => {
                        root.unstack(id);
                    }
                    4 => {
                        root.remove(id);
                    }
                    5 => {
                        let dir = *rng.pick(&[Dir::Left, Dir::Right, Dir::Up, Dir::Down]);
                        root.resize(id, dir, rng.range(0, 400) as i32 - 200, a);
                    }
                    6 => {
                        if let Some(b) = root.borders(a).first().cloned() {
                            root.drag_border(&b, rng.below(400) as u16);
                        }
                    }
                    7 => root.normalize(),
                    8 => {
                        root.expand(id);
                    }
                    9 => {
                        if let Some(preset) = rng.pick(&Preset::ALL).build(&ids, ids.first().copied()) {
                            root = preset;
                        }
                    }
                    _ => {
                        let rects = root.layout(a);
                        for r in rects.values() {
                            assert!(a.union(*r) == a || r.area() == 0, "{r:?} out of {a:?}");
                        }
                        let _ = root.tiles(a, rng.one_in(2));
                        let dir = *rng.pick(&[Dir::Left, Dir::Right, Dir::Up, Dir::Down]);
                        let _ = neighbour(&rects, id, dir, &ids);
                        if let Some((panes, open)) = root.stack_of(id) {
                            let _ = fold(a, panes, open);
                        }
                    }
                }
            }
        });
    }
}
