//! Display layout and pointer mapping.
//!
//! All layout coordinates are *logical points*. A device's displays keep the
//! arrangement the OS gave them; the user positions each **device** as a group
//! with a [`Placement`] offset. Pixels only appear at the injection boundary
//! ([`DisplayInfo::to_pixels`]).

use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use crate::identity::Fingerprint;
use crate::proto::DisplayInfo;

const EPS: f64 = 0.5;
/// How far inside the neighbour the pointer lands after crossing.
pub const ENTRY_INSET: f64 = 2.0;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn opposite(self) -> Self {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }
    pub const ALL: [Side; 4] = [Side::Left, Side::Right, Side::Top, Side::Bottom];
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(&self) -> f64 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }
    /// Half-open: left/top inclusive, right/bottom exclusive, so abutting tiles
    /// cover the plane without a seam.
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn overlap_area(&self, o: &Rect) -> f64 {
        let w = (self.right().min(o.right()) - self.x.max(o.x)).max(0.0);
        let h = (self.bottom().min(o.bottom()) - self.y.max(o.y)).max(0.0);
        w * h
    }
    pub fn translated(&self, dx: f64, dy: f64) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
}

// ───────────────────────────── persisted document ─────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Placement {
    pub device: Fingerprint,
    pub x: i32,
    pub y: i32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisplayRef {
    pub device: Fingerprint,
    pub display: u32,
}

/// Per-boundary override. Missing fields fall back to the global input settings.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct EdgeRule {
    pub device: Fingerprint,
    pub display: u32,
    pub side: Side,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub dwell_ms: Option<u32>,
    #[serde(default)]
    pub require_modifier: Option<bool>,
    #[serde(default)]
    pub block_corners: Option<bool>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct LayoutDoc {
    /// Monotonic; the higher revision wins when peers sync.
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub placements: Vec<Placement>,
    #[serde(default)]
    pub disabled: Vec<DisplayRef>,
    #[serde(default)]
    pub edges: Vec<EdgeRule>,
}

#[derive(Debug, thiserror::Error)]
#[error("layout document is out of bounds")]
pub struct LayoutInvalid;

impl LayoutDoc {
    pub fn validate(&self) -> Result<(), LayoutInvalid> {
        const LIM: i32 = 1_000_000;
        if self.placements.len() > 64 || self.disabled.len() > 256 || self.edges.len() > 1024 {
            return Err(LayoutInvalid);
        }
        if self.placements.iter().any(|p| p.x.abs() > LIM || p.y.abs() > LIM) {
            return Err(LayoutInvalid);
        }
        Ok(())
    }

    pub fn placement(&self, d: &Fingerprint) -> Option<(i32, i32)> {
        self.placements.iter().find(|p| &p.device == d).map(|p| (p.x, p.y))
    }

    pub fn set_placement(&mut self, d: Fingerprint, x: i32, y: i32) {
        match self.placements.iter_mut().find(|p| p.device == d) {
            Some(p) => (p.x, p.y) = (x, y),
            None => self.placements.push(Placement { device: d, x, y }),
        }
    }

    pub fn is_disabled(&self, r: &DisplayRef) -> bool {
        self.disabled.contains(r)
    }

    pub fn set_display_enabled(&mut self, r: DisplayRef, enabled: bool) {
        self.disabled.retain(|d| d != &r);
        if !enabled {
            self.disabled.push(r);
        }
    }

    pub fn rule(&self, device: &Fingerprint, display: u32, side: Side) -> Option<&EdgeRule> {
        self.edges.iter().find(|e| &e.device == device && e.display == display && e.side == side)
    }

    pub fn rule_mut(&mut self, device: Fingerprint, display: u32, side: Side) -> &mut EdgeRule {
        if let Some(i) = self.edges.iter().position(|e| e.device == device && e.display == display && e.side == side) {
            return &mut self.edges[i];
        }
        self.edges.push(EdgeRule { device, display, side, enabled: true, dwell_ms: None, require_modifier: None, block_corners: None });
        self.edges.last_mut().expect("just pushed")
    }

    /// Forget everything about a device (used when trust is revoked).
    pub fn forget(&mut self, d: &Fingerprint) {
        self.placements.retain(|p| &p.device != d);
        self.disabled.retain(|r| &r.device != d);
        self.edges.retain(|e| &e.device != d);
    }
}

// ───────────────────────────── resolved layout ─────────────────────────────

#[derive(Debug, Clone)]
pub struct Tile {
    pub device: Fingerprint,
    pub display: DisplayInfo,
    pub rect: Rect,
    pub enabled: bool,
}

/// An adjacency between two tiles of *different devices*.
#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub a: usize,
    pub b: usize,
    /// Side of tile `a` that touches `b`.
    pub side: Side,
    /// Overlapping range along the shared boundary, in layout coordinates.
    pub span: (f64, f64),
}

#[derive(Debug, Clone, Copy)]
pub struct EdgeDefaults {
    /// Activation zone depth in points.
    pub zone: f64,
    /// Corner dead-zone length in points.
    pub corner: f64,
    pub dwell_ms: u32,
    pub require_modifier: bool,
    pub block_corners: bool,
}

impl Default for EdgeDefaults {
    fn default() -> Self {
        Self { zone: 2.0, corner: 12.0, dwell_ms: 0, require_modifier: false, block_corners: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub from: usize,
    pub to: usize,
    pub side: Side,
    /// Where the pointer lands, in layout coordinates.
    pub entry: (f64, f64),
    pub dwell_ms: u32,
    pub require_modifier: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Probe {
    /// Not pushing against a mapped edge.
    None,
    /// Pushing against an edge that leads to a neighbour.
    Pushing(Hit),
    /// Pushing inside a corner dead-zone.
    Corner,
    /// Edge exists but its rule disables crossing.
    EdgeDisabled,
}

#[derive(Debug, Clone, Default)]
pub struct Desk {
    pub tiles: Vec<Tile>,
    rules: Vec<EdgeRule>,
}

pub fn adjacency(a: &Rect, b: &Rect) -> Option<(Side, (f64, f64))> {
    let y0 = a.y.max(b.y);
    let y1 = a.bottom().min(b.bottom());
    let x0 = a.x.max(b.x);
    let x1 = a.right().min(b.right());
    if (a.right() - b.x).abs() < EPS && y1 - y0 > EPS {
        Some((Side::Right, (y0, y1)))
    } else if (a.x - b.right()).abs() < EPS && y1 - y0 > EPS {
        Some((Side::Left, (y0, y1)))
    } else if (a.bottom() - b.y).abs() < EPS && x1 - x0 > EPS {
        Some((Side::Bottom, (x0, x1)))
    } else if (a.y - b.bottom()).abs() < EPS && x1 - x0 > EPS {
        Some((Side::Top, (x0, x1)))
    } else {
        None
    }
}

impl Desk {
    /// Resolve a layout from the persisted document and the displays each
    /// known device currently reports. Devices without a placement are placed
    /// to the right of everything already on the desk (not persisted until the
    /// user applies).
    pub fn build(doc: &LayoutDoc, devices: &[(Fingerprint, Vec<DisplayInfo>)]) -> Desk {
        let mut tiles: Vec<Tile> = Vec::new();
        let mut unplaced = Vec::new();
        for (fp, displays) in devices {
            match doc.placement(fp) {
                Some((ox, oy)) => push_tiles(&mut tiles, doc, *fp, displays, ox as f64, oy as f64),
                None => unplaced.push((fp, displays)),
            }
        }
        for (fp, displays) in unplaced {
            let (min_x, min_y) = displays.iter().fold((f64::MAX, f64::MAX), |(mx, my), d| (mx.min(d.x as f64), my.min(d.y as f64)));
            let (min_x, min_y) = if displays.is_empty() { (0.0, 0.0) } else { (min_x, min_y) };
            let (target_x, target_y) = if tiles.is_empty() {
                (0.0, 0.0)
            } else {
                let right = tiles.iter().map(|t| t.rect.right()).fold(f64::MIN, f64::max);
                let top = tiles.iter().map(|t| t.rect.y).fold(f64::MAX, f64::min);
                (right, top)
            };
            push_tiles(&mut tiles, doc, *fp, displays, target_x - min_x, target_y - min_y);
        }
        Desk { tiles, rules: doc.edges.clone() }
    }

    pub fn tile_index(&self, device: &Fingerprint, display: u32) -> Option<usize> {
        self.tiles.iter().position(|t| &t.device == device && t.display.id == display)
    }

    pub fn tile_at(&self, x: f64, y: f64) -> Option<usize> {
        self.tiles.iter().position(|t| t.enabled && t.rect.contains(x, y))
    }

    /// Placement offset of a device (layout point of its local origin).
    pub fn device_offset(&self, device: &Fingerprint) -> Option<(f64, f64)> {
        self.tiles.iter().find(|t| &t.device == device).map(|t| (t.rect.x - t.display.x as f64, t.rect.y - t.display.y as f64))
    }

    pub fn device_bounds(&self, device: &Fingerprint) -> Option<Rect> {
        let mut it = self.tiles.iter().filter(|t| &t.device == device);
        let first = it.next()?;
        let (mut x0, mut y0, mut x1, mut y1) = (first.rect.x, first.rect.y, first.rect.right(), first.rect.bottom());
        for t in it {
            x0 = x0.min(t.rect.x);
            y0 = y0.min(t.rect.y);
            x1 = x1.max(t.rect.right());
            y1 = y1.max(t.rect.bottom());
        }
        Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
    }

    /// All boundaries where two enabled tiles of different devices touch.
    pub fn edges(&self) -> Vec<Edge> {
        let mut out = Vec::new();
        for (i, a) in self.tiles.iter().enumerate() {
            for (j, b) in self.tiles.iter().enumerate() {
                if i == j || a.device == b.device || !a.enabled || !b.enabled {
                    continue;
                }
                if let Some((side, span)) = adjacency(&a.rect, &b.rect) {
                    out.push(Edge { a: i, b: j, side, span });
                }
            }
        }
        out
    }

    /// Devices whose tiles overlap tiles of another device (an invalid layout).
    pub fn overlapping_devices(&self) -> Vec<(Fingerprint, Fingerprint)> {
        let mut out = Vec::new();
        for (i, a) in self.tiles.iter().enumerate() {
            for b in &self.tiles[i + 1..] {
                if a.device != b.device && a.enabled && b.enabled && a.rect.overlap_area(&b.rect) > 1.0 {
                    let pair = (a.device.min(b.device), a.device.max(b.device));
                    if !out.contains(&pair) {
                        out.push(pair);
                    }
                }
            }
        }
        out
    }

    /// Devices connected to `start` through touching tiles. Anything else is
    /// unreachable by pointer travel.
    pub fn reachable_from(&self, start: &Fingerprint) -> HashSet<Fingerprint> {
        let mut adj: HashMap<Fingerprint, HashSet<Fingerprint>> = HashMap::new();
        for e in self.edges() {
            adj.entry(self.tiles[e.a].device).or_default().insert(self.tiles[e.b].device);
        }
        let mut seen = HashSet::from([*start]);
        let mut q = VecDeque::from([*start]);
        while let Some(d) = q.pop_front() {
            for n in adj.get(&d).into_iter().flatten() {
                if seen.insert(*n) {
                    q.push_back(*n);
                }
            }
        }
        seen
    }

    fn rule(&self, t: &Tile, side: Side) -> Option<&EdgeRule> {
        self.rules.iter().find(|r| r.device == t.device && r.display == t.display.id && r.side == side)
    }

    /// Is the pointer, at layout position `p` on `tile` and moving by `d`,
    /// pushing against an edge that leads to another device?
    pub fn probe(&self, tile: usize, p: (f64, f64), d: (f64, f64), def: &EdgeDefaults) -> Probe {
        let t = &self.tiles[tile];
        if !t.enabled {
            return Probe::None;
        }
        let r = t.rect;
        // The sides being pushed against, strongest push first.
        let mut pushed: Vec<(Side, f64)> = Vec::new();
        if d.0 > 0.0 && p.0 >= r.right() - def.zone {
            pushed.push((Side::Right, d.0));
        }
        if d.0 < 0.0 && p.0 < r.x + def.zone {
            pushed.push((Side::Left, -d.0));
        }
        if d.1 > 0.0 && p.1 >= r.bottom() - def.zone {
            pushed.push((Side::Bottom, d.1));
        }
        if d.1 < 0.0 && p.1 < r.y + def.zone {
            pushed.push((Side::Top, -d.1));
        }
        pushed.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut result = Probe::None;
        for (side, _) in pushed {
            let along = if matches!(side, Side::Left | Side::Right) { p.1 } else { p.0 };
            let (lo, hi) = if matches!(side, Side::Left | Side::Right) { (r.y, r.bottom()) } else { (r.x, r.right()) };
            let rule = self.rule(t, side);
            let block_corners = rule.and_then(|r| r.block_corners).unwrap_or(def.block_corners);
            if block_corners && (along - lo < def.corner || hi - along <= def.corner) {
                if result == Probe::None {
                    result = Probe::Corner;
                }
                continue;
            }
            let Some(to) = self.neighbour_at(tile, side, along) else { continue };
            if rule.is_some_and(|r| !r.enabled) {
                result = Probe::EdgeDisabled;
                continue;
            }
            let n = &self.tiles[to].rect;
            let entry = match side {
                Side::Right => (n.x + ENTRY_INSET, along.clamp(n.y, n.bottom() - 1.0)),
                Side::Left => (n.right() - ENTRY_INSET - 1.0, along.clamp(n.y, n.bottom() - 1.0)),
                Side::Bottom => (along.clamp(n.x, n.right() - 1.0), n.y + ENTRY_INSET),
                Side::Top => (along.clamp(n.x, n.right() - 1.0), n.bottom() - ENTRY_INSET - 1.0),
            };
            return Probe::Pushing(Hit {
                from: tile,
                to,
                side,
                entry,
                dwell_ms: rule.and_then(|r| r.dwell_ms).unwrap_or(def.dwell_ms),
                require_modifier: rule.and_then(|r| r.require_modifier).unwrap_or(def.require_modifier),
            });
        }
        result
    }

    /// Enabled tile of a *different device* touching `side` of `tile` at `along`.
    fn neighbour_at(&self, tile: usize, side: Side, along: f64) -> Option<usize> {
        let a = &self.tiles[tile];
        self.tiles.iter().enumerate().find_map(|(j, b)| {
            if j == tile || b.device == a.device || !b.enabled {
                return None;
            }
            let (s, (lo, hi)) = adjacency(&a.rect, &b.rect)?;
            (s == side && along >= lo && along < hi).then_some(j)
        })
    }

    /// Move the virtual pointer by `d` through the mapped tiles. Every sub-step
    /// of at most one point must land inside an enabled tile, so the pointer
    /// can never jump across an unmapped gap; against a wall it slides along it.
    pub fn step(&self, mut tile: usize, mut p: (f64, f64), d: (f64, f64)) -> (usize, (f64, f64)) {
        let n = d.0.abs().max(d.1.abs()).ceil().clamp(1.0, 4096.0) as usize;
        let (sx, sy) = (d.0 / n as f64, d.1 / n as f64);
        let mut blocked_x = false;
        let mut blocked_y = false;
        for _ in 0..n {
            let candidates = [(p.0 + sx, p.1 + sy), (p.0 + sx, p.1), (p.0, p.1 + sy)];
            let mut moved = false;
            for (i, c) in candidates.iter().enumerate() {
                if (i == 1 && (blocked_x || sx == 0.0)) || (i == 2 && (blocked_y || sy == 0.0)) {
                    continue;
                }
                if let Some(t) = self.tile_at(c.0, c.1) {
                    // A diagonal step needs a 4-connected path, otherwise the
                    // pointer could slip between tiles that only share a corner.
                    let pinched = i == 0 && sx != 0.0 && sy != 0.0 && self.tile_at(p.0 + sx, p.1).is_none() && self.tile_at(p.0, p.1 + sy).is_none();
                    if pinched {
                        continue;
                    }
                    tile = t;
                    p = *c;
                    moved = true;
                    break;
                }
            }
            if !moved {
                blocked_x = true;
                blocked_y = true;
            }
            if blocked_x && blocked_y {
                break;
            }
        }
        (tile, p)
    }

    pub fn center_of_device(&self, device: &Fingerprint) -> Option<(usize, (f64, f64))> {
        let (i, t) = self.tiles.iter().enumerate().filter(|(_, t)| &t.device == device && t.enabled).min_by_key(|(_, t)| !t.display.primary)?;
        Some((i, (t.rect.x + t.rect.w / 2.0, t.rect.y + t.rect.h / 2.0)))
    }
}

fn push_tiles(tiles: &mut Vec<Tile>, doc: &LayoutDoc, fp: Fingerprint, displays: &[DisplayInfo], ox: f64, oy: f64) {
    for d in displays {
        tiles.push(Tile {
            device: fp,
            rect: Rect::new(d.x as f64 + ox, d.y as f64 + oy, d.width as f64, d.height as f64),
            enabled: !doc.is_disabled(&DisplayRef { device: fp, display: d.id }),
            display: d.clone(),
        });
    }
}

// ───────────────────────────── editor helpers ─────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Guide {
    pub vertical: bool,
    pub pos: f64,
    pub from: f64,
    pub to: f64,
}

/// Snap a moving group of rects against fixed rects. Returns the extra offset
/// to apply and the guides to draw. Abutting edges win over alignment.
pub fn snap(moving: &[Rect], fixed: &[Rect], threshold: f64) -> (f64, f64, Vec<Guide>) {
    let Some(bounds) = bounds_of(moving) else { return (0.0, 0.0, vec![]) };
    let mut best_x: Option<(f64, f64, f64, bool)> = None; // dist, pos, other idx unused, abut
    let mut best_y: Option<(f64, f64, f64, bool)> = None;
    let mxs = [(bounds.x, false), (bounds.right(), false), (bounds.x + bounds.w / 2.0, false)];
    let mys = [(bounds.y, false), (bounds.bottom(), false), (bounds.y + bounds.h / 2.0, false)];
    for f in fixed {
        let fxs = [(f.x, 0), (f.right(), 1), (f.x + f.w / 2.0, 2)];
        let fys = [(f.y, 0), (f.bottom(), 1), (f.y + f.h / 2.0, 2)];
        for (mi, (m, _)) in mxs.iter().enumerate() {
            for (fi, (o, _)) in fxs.iter().enumerate() {
                let abut = (mi == 0 && fi == 1) || (mi == 1 && fi == 0);
                let dist = o - m;
                if dist.abs() <= threshold && better(&best_x, dist, abut) {
                    best_x = Some((dist, *o, 0.0, abut));
                }
            }
        }
        for (mi, (m, _)) in mys.iter().enumerate() {
            for (fi, (o, _)) in fys.iter().enumerate() {
                let abut = (mi == 0 && fi == 1) || (mi == 1 && fi == 0);
                let dist = o - m;
                if dist.abs() <= threshold && better(&best_y, dist, abut) {
                    best_y = Some((dist, *o, 0.0, abut));
                }
            }
        }
    }
    let dx = best_x.map_or(0.0, |b| b.0);
    let dy = best_y.map_or(0.0, |b| b.0);
    let snapped = bounds.translated(dx, dy);
    let mut guides = Vec::new();
    if let Some((_, pos, _, _)) = best_x {
        let (from, to) = span_with_fixed(fixed, &snapped, true, pos);
        guides.push(Guide { vertical: true, pos, from, to });
    }
    if let Some((_, pos, _, _)) = best_y {
        let (from, to) = span_with_fixed(fixed, &snapped, false, pos);
        guides.push(Guide { vertical: false, pos, from, to });
    }
    (dx, dy, guides)
}

fn better(cur: &Option<(f64, f64, f64, bool)>, dist: f64, abut: bool) -> bool {
    match cur {
        None => true,
        Some((d, _, _, a)) => (abut && !a) || (abut == *a && dist.abs() < d.abs()),
    }
}

fn span_with_fixed(fixed: &[Rect], moving: &Rect, vertical: bool, pos: f64) -> (f64, f64) {
    let (mut lo, mut hi) = if vertical { (moving.y, moving.bottom()) } else { (moving.x, moving.right()) };
    for f in fixed {
        let hit = if vertical {
            (f.x - pos).abs() < EPS || (f.right() - pos).abs() < EPS || (f.x + f.w / 2.0 - pos).abs() < EPS
        } else {
            (f.y - pos).abs() < EPS || (f.bottom() - pos).abs() < EPS || (f.y + f.h / 2.0 - pos).abs() < EPS
        };
        if hit {
            let (a, b) = if vertical { (f.y, f.bottom()) } else { (f.x, f.right()) };
            lo = lo.min(a);
            hi = hi.max(b);
        }
    }
    (lo, hi)
}

pub fn bounds_of(rects: &[Rect]) -> Option<Rect> {
    let mut it = rects.iter();
    let f = it.next()?;
    let (mut x0, mut y0, mut x1, mut y1) = (f.x, f.y, f.right(), f.bottom());
    for r in it {
        x0 = x0.min(r.x);
        y0 = y0.min(r.y);
        x1 = x1.max(r.right());
        y1 = y1.max(r.bottom());
    }
    Some(Rect::new(x0, y0, x1 - x0, y1 - y0))
}

// ───────────────────────────── scale conversion ─────────────────────────────

impl DisplayInfo {
    /// Display-local points → display-local pixels.
    pub fn to_pixels(&self, x: f32, y: f32) -> (i32, i32) {
        ((x * self.scale).round() as i32, (y * self.scale).round() as i32)
    }
    pub fn from_pixels(&self, px: i32, py: i32) -> (f32, f32) {
        (px as f32 / self.scale, py as f32 / self.scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(n: u8) -> Fingerprint {
        Fingerprint([n; 32])
    }
    fn disp(id: u32, x: i32, y: i32, w: u32, h: u32, scale: f32) -> DisplayInfo {
        DisplayInfo { id, name: format!("D{id}"), x, y, width: w, height: h, scale, rotation: 0, primary: id == 1 }
    }
    /// A: 1440x900 @2x at origin; B right of A, 1920x1080 @1x.
    fn two_devices() -> (Desk, Fingerprint, Fingerprint) {
        let (a, b) = (fp(1), fp(2));
        let mut doc = LayoutDoc::default();
        doc.set_placement(a, 0, 0);
        doc.set_placement(b, 1440, 0);
        let desk = Desk::build(&doc, &[(a, vec![disp(1, 0, 0, 1440, 900, 2.0)]), (b, vec![disp(1, 0, 0, 1920, 1080, 1.0)])]);
        (desk, a, b)
    }
    fn tile(desk: &Desk, d: &Fingerprint) -> usize {
        desk.tile_index(d, 1).unwrap()
    }

    #[test]
    fn adjacency_reports_side_and_overlap_span() {
        let (desk, _, _) = two_devices();
        let e = desk.edges();
        assert_eq!(e.len(), 2); // A→B and B→A
        let ab = e.iter().find(|e| e.a == 0).unwrap();
        assert_eq!(ab.side, Side::Right);
        assert_eq!(ab.span, (0.0, 900.0)); // only A's height overlaps
    }

    #[test]
    fn pushing_right_edge_enters_neighbour_at_matching_height() {
        let (desk, a, b) = two_devices();
        let Probe::Pushing(h) = desk.probe(tile(&desk, &a), (1439.0, 450.0), (5.0, 0.0), &EdgeDefaults::default()) else { panic!("expected push") };
        assert_eq!(desk.tiles[h.to].device, b);
        assert_eq!(h.side, Side::Right);
        assert_eq!(h.entry, (1440.0 + ENTRY_INSET, 450.0));
    }

    #[test]
    fn no_crossing_when_not_pushing_outward_or_not_at_edge() {
        let (desk, a, _) = two_devices();
        let t = tile(&desk, &a);
        let d = EdgeDefaults::default();
        assert_eq!(desk.probe(t, (1439.0, 450.0), (-5.0, 0.0), &d), Probe::None);
        assert_eq!(desk.probe(t, (700.0, 450.0), (5.0, 0.0), &d), Probe::None);
        assert_eq!(desk.probe(t, (1439.0, 450.0), (0.0, 0.0), &d), Probe::None);
    }

    #[test]
    fn corners_are_dead_zones_by_default_and_configurable() {
        let (desk, a, _) = two_devices();
        let t = tile(&desk, &a);
        assert_eq!(desk.probe(t, (1439.0, 3.0), (5.0, 0.0), &EdgeDefaults::default()), Probe::Corner);
        assert_eq!(desk.probe(t, (1439.0, 897.0), (5.0, 0.0), &EdgeDefaults::default()), Probe::Corner);
        let open = EdgeDefaults { block_corners: false, ..EdgeDefaults::default() };
        assert!(matches!(desk.probe(t, (1439.0, 3.0), (5.0, 0.0), &open), Probe::Pushing(_)));
    }

    #[test]
    fn pointer_beyond_neighbour_span_has_no_target() {
        // B is only 700 tall and sits at y=0; pointer at y=800 on A has nowhere to go.
        let (a, b) = (fp(1), fp(2));
        let mut doc = LayoutDoc::default();
        doc.set_placement(a, 0, 0);
        doc.set_placement(b, 1440, 0);
        let desk = Desk::build(&doc, &[(a, vec![disp(1, 0, 0, 1440, 900, 1.0)]), (b, vec![disp(1, 0, 0, 1000, 700, 1.0)])]);
        assert_eq!(desk.probe(0, (1439.0, 800.0), (5.0, 0.0), &EdgeDefaults::default()), Probe::None);
        assert!(matches!(desk.probe(0, (1439.0, 300.0), (5.0, 0.0), &EdgeDefaults::default()), Probe::Pushing(_)));
    }

    #[test]
    fn edge_rule_overrides_apply() {
        let (a, b) = (fp(1), fp(2));
        let mut doc = LayoutDoc::default();
        doc.set_placement(a, 0, 0);
        doc.set_placement(b, 1440, 0);
        let r = doc.rule_mut(a, 1, Side::Right);
        r.dwell_ms = Some(250);
        r.require_modifier = Some(true);
        let devs = [(a, vec![disp(1, 0, 0, 1440, 900, 1.0)]), (b, vec![disp(1, 0, 0, 800, 600, 1.0)])];
        let desk = Desk::build(&doc, &devs);
        let Probe::Pushing(h) = desk.probe(0, (1439.0, 300.0), (5.0, 0.0), &EdgeDefaults::default()) else { panic!() };
        assert_eq!((h.dwell_ms, h.require_modifier), (250, true));
        doc.rule_mut(a, 1, Side::Right).enabled = false;
        let desk = Desk::build(&doc, &devs);
        assert_eq!(desk.probe(0, (1439.0, 300.0), (5.0, 0.0), &EdgeDefaults::default()), Probe::EdgeDisabled);
    }

    #[test]
    fn excluded_monitor_is_not_a_target_and_not_traversable() {
        let (a, b) = (fp(1), fp(2));
        let mut doc = LayoutDoc::default();
        doc.set_placement(a, 0, 0);
        doc.set_placement(b, 1440, 0);
        doc.set_display_enabled(DisplayRef { device: b, display: 1 }, false);
        let desk = Desk::build(&doc, &[(a, vec![disp(1, 0, 0, 1440, 900, 1.0)]), (b, vec![disp(1, 0, 0, 800, 600, 1.0)])]);
        assert_eq!(desk.probe(0, (1439.0, 300.0), (5.0, 0.0), &EdgeDefaults::default()), Probe::None);
        assert!(desk.edges().is_empty());
        assert_eq!(desk.tile_at(1500.0, 100.0), None);
    }

    #[test]
    fn step_never_teleports_across_a_gap() {
        // 100-point gap between the two displays.
        let (a, b) = (fp(1), fp(2));
        let mut doc = LayoutDoc::default();
        doc.set_placement(a, 0, 0);
        doc.set_placement(b, 1540, 0);
        let desk = Desk::build(&doc, &[(a, vec![disp(1, 0, 0, 1440, 900, 1.0)]), (b, vec![disp(1, 0, 0, 800, 600, 1.0)])]);
        let (t, p) = desk.step(0, (1430.0, 300.0), (500.0, 0.0));
        assert_eq!(desk.tiles[t].device, a);
        assert!(p.0 < 1440.0, "pointer must stop at the unmapped gap, got {p:?}");
        assert!(desk.edges().is_empty());
        assert!(!desk.reachable_from(&a).contains(&b));
    }

    #[test]
    fn step_crosses_touching_tiles_and_slides_along_walls() {
        let (desk, a, b) = two_devices();
        let (t, p) = desk.step(tile(&desk, &a), (1430.0, 450.0), (30.0, 0.0));
        assert_eq!(desk.tiles[t].device, b);
        assert!((p.0 - 1460.0).abs() < 1e-6);
        // Moving diagonally into the top wall slides along it.
        let (t, p) = desk.step(tile(&desk, &b), (1700.0, 5.0), (20.0, -50.0));
        assert_eq!(desk.tiles[t].device, b);
        assert!(p.1 >= 0.0 && p.0 > 1700.0, "should slide in x while y is blocked: {p:?}");
    }

    #[test]
    fn corner_to_corner_diagonal_does_not_skip_through_a_pinch_point() {
        // L-shaped: A at origin, B below-right sharing only a corner point.
        let (a, b) = (fp(1), fp(2));
        let mut doc = LayoutDoc::default();
        doc.set_placement(a, 0, 0);
        doc.set_placement(b, 100, 100);
        let desk = Desk::build(&doc, &[(a, vec![disp(1, 0, 0, 100, 100, 1.0)]), (b, vec![disp(1, 0, 0, 100, 100, 1.0)])]);
        assert!(desk.edges().is_empty(), "corner touching is not an edge");
        let (t, _) = desk.step(0, (99.0, 99.0), (30.0, 30.0));
        assert_eq!(desk.tiles[t].device, a);
    }

    #[test]
    fn mixed_dpi_points_and_pixels_round_trip() {
        let d = disp(1, 0, 0, 1440, 900, 2.0);
        assert_eq!(d.pixel_size(), (2880, 1800));
        assert_eq!(d.to_pixels(100.25, 50.5), (201, 101));
        let (x, y) = d.from_pixels(200, 100);
        assert_eq!((x, y), (100.0, 50.0));
        let low = disp(1, 0, 0, 1920, 1080, 1.0);
        assert_eq!(low.to_pixels(1919.0, 1079.0), (1919, 1079));
    }

    #[test]
    fn unplaced_devices_go_to_the_right_and_overlaps_are_detected() {
        let (a, b) = (fp(1), fp(2));
        let mut doc = LayoutDoc::default();
        doc.set_placement(a, 0, 0);
        let devs = [(a, vec![disp(1, 0, 0, 1000, 800, 1.0)]), (b, vec![disp(1, 0, 0, 500, 500, 1.0)])];
        let desk = Desk::build(&doc, &devs);
        assert_eq!(desk.device_bounds(&b).unwrap().x, 1000.0);
        assert!(desk.reachable_from(&a).contains(&b));
        assert!(desk.overlapping_devices().is_empty());
        doc.set_placement(b, 500, 100);
        assert_eq!(Desk::build(&doc, &devs).overlapping_devices().len(), 1);
    }

    #[test]
    fn multi_monitor_devices_keep_their_internal_arrangement() {
        let (a, b) = (fp(1), fp(2));
        let mut doc = LayoutDoc::default();
        doc.set_placement(a, 0, 0);
        doc.set_placement(b, 1000, 0);
        let devs = [(a, vec![disp(1, 0, 0, 1000, 800, 1.0)]), (b, vec![disp(1, 0, 0, 800, 600, 1.0), disp(2, 800, 0, 800, 600, 1.0)])];
        let desk = Desk::build(&doc, &devs);
        // Only B's left monitor touches A.
        let edges = desk.edges();
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| desk.tiles[e.a].display.id == 1 && desk.tiles[e.b].display.id == 1));
        // Pointer travels from B1 to B2 through the same-device boundary.
        let (t, p) = desk.step(desk.tile_index(&b, 1).unwrap(), (1790.0, 100.0), (30.0, 0.0));
        assert_eq!(desk.tiles[t].display.id, 2);
        assert!(p.0 > 1800.0);
    }

    #[test]
    fn snapping_abuts_edges_and_prefers_abutment_over_alignment() {
        let fixed = [Rect::new(0.0, 0.0, 1000.0, 800.0)];
        let moving = [Rect::new(1012.0, 7.0, 500.0, 500.0)];
        let (dx, dy, guides) = snap(&moving, &fixed, 16.0);
        assert_eq!((dx, dy), (-12.0, -7.0));
        assert_eq!(guides.len(), 2);
        let (dx, dy, g) = snap(&[Rect::new(5000.0, 5000.0, 10.0, 10.0)], &fixed, 16.0);
        assert_eq!((dx, dy, g.len()), (0.0, 0.0, 0));
    }

    #[test]
    fn layout_doc_validation_and_forget() {
        let mut doc = LayoutDoc::default();
        doc.set_placement(fp(1), 10, 10);
        doc.rule_mut(fp(1), 1, Side::Left).enabled = false;
        assert!(doc.validate().is_ok());
        doc.forget(&fp(1));
        assert!(doc.placements.is_empty() && doc.edges.is_empty());
        doc.set_placement(fp(2), 5_000_000, 0);
        assert!(doc.validate().is_err());
    }
}
