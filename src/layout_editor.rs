//! Logic behind the drag-to-arrange screen: a *draft* layout that the user can
//! experiment with, with snapping, validity checks, reachability warnings and
//! per-edge rules. Nothing here touches the applied layout until `draft()` is
//! sent with Apply, so experimenting never breaks a working setup.

use crate::geometry::{Desk, DisplayRef, Guide, LayoutDoc, Rect, Side, bounds_of, snap};
use crate::identity::Fingerprint;
use crate::proto::DisplayInfo;

/// Snap distance in layout points.
const SNAP: f64 = 28.0;

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub fp: Fingerprint,
    pub label: String,
    pub local: bool,
    pub online: bool,
    pub displays: Vec<DisplayInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TileView {
    pub index: usize,
    pub device: usize,
    pub label: String,
    pub device_label: String,
    pub detail: String,
    pub rect: Rect,
    pub local: bool,
    pub enabled: bool,
    pub online: bool,
    pub invalid: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GroupView {
    pub index: usize,
    pub label: String,
    pub rect: Rect,
    pub local: bool,
    pub online: bool,
    pub invalid: bool,
    pub unreachable: bool,
    pub active: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeView {
    pub side: Side,
    pub neighbor: String,
    pub span: (f64, f64),
    pub enabled: bool,
    pub dwell_ms: u32,
    pub modifier: bool,
    pub corners: bool,
}

/// Global defaults shown for edges that have no override.
#[derive(Debug, Clone, Copy)]
pub struct Defaults {
    pub dwell_ms: u32,
    pub require_modifier: bool,
    pub block_corners: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub text: String,
    /// Blocks Apply (overlap) or merely warns (unreachable).
    pub blocking: bool,
}

pub struct Editor {
    devices: Vec<DeviceInfo>,
    applied: LayoutDoc,
    draft: LayoutDoc,
    dirty: bool,
    selected: Option<usize>,
    active: Option<Fingerprint>,
    pub defaults: Defaults,
}

impl Editor {
    pub fn new(defaults: Defaults) -> Self {
        Self { devices: vec![], applied: LayoutDoc::default(), draft: LayoutDoc::default(), dirty: false, selected: None, active: None, defaults }
    }

    /// Feed the latest engine state. While the user has unapplied changes the
    /// draft is kept; otherwise it follows the applied layout.
    pub fn sync(&mut self, devices: Vec<DeviceInfo>, applied: &LayoutDoc, active: Option<Fingerprint>, defaults: Defaults) {
        self.devices = devices;
        self.defaults = defaults;
        self.active = active;
        if !self.dirty || applied.revision != self.applied.revision {
            // Someone applied a layout (here or via a peer): follow it and drop stale experiments.
            self.draft = applied.clone();
            self.dirty = false;
        }
        self.applied = applied.clone();
        if self.selected.is_some_and(|t| t >= self.desk().tiles.len()) {
            self.selected = None;
        }
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn draft(&self) -> LayoutDoc {
        // Materialise the automatic placements the user has been looking at, so
        // Apply stores exactly what was on screen.
        let desk = self.desk();
        let mut doc = self.draft.clone();
        for d in &self.devices {
            if doc.placement(&d.fp).is_none()
                && let Some((x, y)) = desk.device_offset(&d.fp)
            {
                doc.set_placement(d.fp, x.round() as i32, y.round() as i32);
            }
        }
        doc
    }

    pub fn revert(&mut self) {
        self.draft = self.applied.clone();
        self.dirty = false;
    }

    /// Forget manual placement: devices line up left to right again.
    pub fn reset_auto(&mut self) {
        self.draft.placements.clear();
        self.dirty = true;
    }

    pub fn desk(&self) -> Desk {
        let devs: Vec<_> = self.devices.iter().filter(|d| !d.displays.is_empty()).map(|d| (d.fp, d.displays.clone())).collect();
        Desk::build(&self.draft, &devs)
    }

    fn device_index(&self, fp: &Fingerprint) -> Option<usize> {
        self.devices.iter().position(|d| &d.fp == fp)
    }

    fn overlapping(&self, desk: &Desk) -> Vec<Fingerprint> {
        desk.overlapping_devices().into_iter().flat_map(|(a, b)| [a, b]).collect()
    }

    pub fn tiles(&self) -> Vec<TileView> {
        let desk = self.desk();
        let bad = self.overlapping(&desk);
        desk.tiles
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let di = self.device_index(&t.device).unwrap_or(0);
                let dev = &self.devices[di];
                let (pw, ph) = t.display.pixel_size();
                TileView {
                    index: i,
                    device: di,
                    label: if t.display.name.is_empty() { format!("Display {}", t.display.id) } else { t.display.name.clone() },
                    device_label: dev.label.clone(),
                    detail: format!("{pw}×{ph} · {}×", trim_scale(t.display.scale))
                        + &if t.display.rotation != 0 { format!(" · {}°", t.display.rotation) } else { String::new() },
                    rect: t.rect,
                    local: dev.local,
                    enabled: t.enabled,
                    online: dev.online,
                    invalid: bad.contains(&t.device),
                }
            })
            .collect()
    }

    pub fn groups(&self) -> Vec<GroupView> {
        let desk = self.desk();
        let local = self.devices.iter().find(|d| d.local).map(|d| d.fp);
        let reach = local.map(|l| desk.reachable_from(&l));
        let bad = self.overlapping(&desk);
        self.devices
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let rect = desk.device_bounds(&d.fp)?;
                Some(GroupView {
                    index: i,
                    label: d.label.clone(),
                    rect,
                    local: d.local,
                    online: d.online,
                    invalid: bad.contains(&d.fp),
                    unreachable: reach.as_ref().is_some_and(|r| !r.contains(&d.fp)),
                    active: self.active == Some(d.fp),
                })
            })
            .collect()
    }

    pub fn bounds(&self) -> Rect {
        let rects: Vec<Rect> = self.tiles().iter().map(|t| t.rect).collect();
        bounds_of(&rects).unwrap_or(Rect::new(0.0, 0.0, 1000.0, 600.0))
    }

    /// Snap guides while dragging device `device` by (dx, dy) points.
    pub fn drag_preview(&self, device: usize, dx: f64, dy: f64) -> Vec<Guide> {
        let (moving, fixed) = self.rects_split(device);
        let moved: Vec<Rect> = moving.iter().map(|r| r.translated(dx, dy)).collect();
        snap(&moved, &fixed, SNAP).2
    }

    fn rects_split(&self, device: usize) -> (Vec<Rect>, Vec<Rect>) {
        let fp = self.devices[device].fp;
        let desk = self.desk();
        let mut moving = vec![];
        let mut fixed = vec![];
        for t in desk.tiles.iter().filter(|t| t.enabled) {
            if t.device == fp {
                moving.push(t.rect);
            } else {
                fixed.push(t.rect);
            }
        }
        (moving, fixed)
    }

    /// The user dropped a device after dragging it by (dx, dy) points.
    pub fn drop_device(&mut self, device: usize, dx: f64, dy: f64) {
        let Some(d) = self.devices.get(device) else { return };
        let fp = d.fp;
        let (moving, fixed) = self.rects_split(device);
        let moved: Vec<Rect> = moving.iter().map(|r| r.translated(dx, dy)).collect();
        let (sx, sy, _) = snap(&moved, &fixed, SNAP);
        let Some((ox, oy)) = self.desk().device_offset(&fp) else { return };
        self.draft.set_placement(fp, (ox + dx + sx).round() as i32, (oy + dy + sy).round() as i32);
        self.dirty = true;
    }

    /// Keyboard alternative to dragging.
    pub fn nudge(&mut self, tile: usize, dx: f64, dy: f64) {
        let desk = self.desk();
        let Some(t) = desk.tiles.get(tile) else { return };
        let Some(device) = self.device_index(&t.device) else { return };
        let Some((ox, oy)) = desk.device_offset(&t.device) else { return };
        self.draft.set_placement(t.device, (ox + dx).round() as i32, (oy + dy).round() as i32);
        self.dirty = true;
        let _ = device;
    }

    pub fn select(&mut self, tile: Option<usize>) {
        self.selected = tile;
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn toggle_tile(&mut self, tile: usize, enabled: bool) {
        let desk = self.desk();
        let Some(t) = desk.tiles.get(tile) else { return };
        self.draft.set_display_enabled(DisplayRef { device: t.device, display: t.display.id }, enabled);
        self.dirty = true;
    }

    pub fn problems(&self) -> Vec<Problem> {
        let desk = self.desk();
        let mut out = Vec::new();
        for (a, b) in desk.overlapping_devices() {
            let (la, lb) = (self.label_of(&a), self.label_of(&b));
            out.push(Problem { text: format!("{la} and {lb} overlap. Move one so their screens only touch."), blocking: true });
        }
        if let Some(local) = self.devices.iter().find(|d| d.local) {
            let reach = desk.reachable_from(&local.fp);
            for d in &self.devices {
                if !d.displays.is_empty() && !reach.contains(&d.fp) {
                    out.push(Problem {
                        text: format!(
                            "{} does not touch any screen connected to this computer, so the pointer cannot reach it. Drag it against another screen.",
                            d.label
                        ),
                        blocking: false,
                    });
                }
            }
        }
        out
    }

    pub fn blocking(&self) -> bool {
        self.problems().iter().any(|p| p.blocking)
    }

    fn label_of(&self, fp: &Fingerprint) -> String {
        self.devices.iter().find(|d| &d.fp == fp).map(|d| d.label.clone()).unwrap_or_default()
    }

    /// Boundaries of the selected tile with its neighbours on other devices.
    pub fn edges_of(&self, tile: usize) -> Vec<EdgeView> {
        let desk = self.desk();
        let Some(t) = desk.tiles.get(tile) else { return vec![] };
        desk.edges()
            .into_iter()
            .filter(|e| e.a == tile)
            .map(|e| {
                let rule = self.draft.rule(&t.device, t.display.id, e.side);
                EdgeView {
                    side: e.side,
                    neighbor: format!("{} · {}", self.label_of(&desk.tiles[e.b].device), desk.tiles[e.b].display.name),
                    span: e.span,
                    enabled: rule.is_none_or(|r| r.enabled),
                    dwell_ms: rule.and_then(|r| r.dwell_ms).unwrap_or(self.defaults.dwell_ms),
                    modifier: rule.and_then(|r| r.require_modifier).unwrap_or(self.defaults.require_modifier),
                    corners: rule.and_then(|r| r.block_corners).unwrap_or(self.defaults.block_corners),
                }
            })
            .collect()
    }

    pub fn set_edge(&mut self, tile: usize, edge: usize, enabled: bool, dwell_ms: u32, modifier: bool, corners: bool) {
        let edges = self.edges_of(tile);
        let desk = self.desk();
        let (Some(e), Some(t)) = (edges.get(edge), desk.tiles.get(tile)) else { return };
        let r = self.draft.rule_mut(t.device, t.display.id, e.side);
        r.enabled = enabled;
        r.dwell_ms = Some(dwell_ms);
        r.require_modifier = Some(modifier);
        r.block_corners = Some(corners);
        self.dirty = true;
    }

    pub fn tile_title(&self, tile: usize) -> Option<(String, String)> {
        let desk = self.desk();
        let t = desk.tiles.get(tile)?;
        let dev = self.devices.iter().find(|d| d.fp == t.device)?;
        let main = if t.display.primary { ", main display" } else { "" };
        Some((
            format!("{} · {}", dev.label, t.display.name),
            format!("{}×{} points, {}× scale{main}", t.display.width, t.display.height, trim_scale(t.display.scale)),
        ))
    }
}

fn trim_scale(s: f32) -> String {
    if (s - s.round()).abs() < 0.01 { format!("{}", s.round() as i32) } else { format!("{s:.2}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(n: u8) -> Fingerprint {
        Fingerprint([n; 32])
    }
    fn disp(id: u32, x: i32, w: u32, h: u32) -> DisplayInfo {
        DisplayInfo { id, name: format!("Display {id}"), x, y: 0, width: w, height: h, scale: 2.0, rotation: 0, primary: id == 1 }
    }
    fn dev(n: u8, label: &str, local: bool, displays: Vec<DisplayInfo>) -> DeviceInfo {
        DeviceInfo { fp: fp(n), label: label.into(), local, online: true, displays }
    }
    fn defaults() -> Defaults {
        Defaults { dwell_ms: 0, require_modifier: false, block_corners: true }
    }
    fn editor() -> Editor {
        let mut e = Editor::new(defaults());
        let mut applied = LayoutDoc::default();
        applied.set_placement(fp(1), 0, 0);
        applied.set_placement(fp(2), 1440, 0);
        applied.revision = 1;
        e.sync(vec![dev(1, "Mac", true, vec![disp(1, 0, 1440, 900)]), dev(2, "PC", false, vec![disp(1, 0, 1920, 1080)])], &applied, None, defaults());
        e
    }

    #[test]
    fn starts_clean_and_lists_tiles_and_groups() {
        let e = editor();
        assert!(!e.dirty());
        let tiles = e.tiles();
        assert_eq!(tiles.len(), 2);
        assert_eq!(tiles[0].detail, "2880×1800 · 2×");
        assert!(tiles[0].local && !tiles[1].local);
        let groups = e.groups();
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| !g.unreachable && !g.invalid));
        assert!(e.problems().is_empty());
    }

    #[test]
    fn dropping_near_an_edge_snaps_so_screens_touch() {
        let mut e = editor();
        // Drag the PC 15 points right and 10 down from its touching position.
        e.drop_device(1, 15.0, 10.0);
        assert!(e.dirty());
        let pc = e.tiles().into_iter().find(|t| !t.local).unwrap();
        assert_eq!((pc.rect.x, pc.rect.y), (1440.0, 0.0), "snapped back flush to the Mac's right edge and top");
        assert!(e.problems().is_empty());
    }

    #[test]
    fn overlap_is_flagged_and_blocks_apply_without_touching_the_applied_layout() {
        let mut e = editor();
        e.drop_device(1, -700.0, 100.0);
        let probs = e.problems();
        assert!(probs.iter().any(|p| p.blocking && p.text.contains("overlap")), "{probs:?}");
        assert!(e.blocking());
        assert!(e.tiles().iter().all(|t| t.invalid));
        // Revert restores the working layout.
        e.revert();
        assert!(!e.dirty() && e.problems().is_empty());
    }

    #[test]
    fn a_gap_makes_a_device_unreachable_and_says_so_but_allows_apply() {
        let mut e = editor();
        e.drop_device(1, 600.0, 0.0);
        let probs = e.problems();
        assert!(probs.iter().any(|p| !p.blocking && p.text.contains("cannot reach")), "{probs:?}");
        assert!(!e.blocking());
        assert!(e.groups().iter().any(|g| g.unreachable));
    }

    #[test]
    fn keyboard_nudging_moves_by_points() {
        let mut e = editor();
        let before = e.tiles()[1].rect;
        e.nudge(1, 10.0, -5.0);
        let after = e.tiles()[1].rect;
        assert_eq!((after.x - before.x, after.y - before.y), (10.0, -5.0));
    }

    #[test]
    fn excluding_a_monitor_removes_the_edge_and_is_part_of_the_draft() {
        let mut e = editor();
        assert_eq!(e.edges_of(0).len(), 1);
        e.toggle_tile(1, false);
        assert!(e.edges_of(0).is_empty());
        assert!(e.draft().is_disabled(&DisplayRef { device: fp(2), display: 1 }));
        assert!(!e.tiles()[1].enabled);
    }

    #[test]
    fn edge_rules_are_edited_per_boundary_with_defaults_shown() {
        let mut e = editor();
        let edge = e.edges_of(0).remove(0);
        assert_eq!((edge.side, edge.enabled, edge.dwell_ms, edge.modifier, edge.corners), (Side::Right, true, 0, false, true));
        assert_eq!(edge.span, (0.0, 900.0));
        e.set_edge(0, 0, true, 300, true, false);
        let again = e.edges_of(0).remove(0);
        assert_eq!((again.dwell_ms, again.modifier, again.corners), (300, true, false));
        let doc = e.draft();
        let r = doc.rule(&fp(1), 1, Side::Right).unwrap();
        assert_eq!((r.dwell_ms, r.require_modifier, r.block_corners), (Some(300), Some(true), Some(false)));
    }

    #[test]
    fn drag_preview_offers_guides_and_a_new_device_is_auto_placed_then_materialised() {
        let mut e = Editor::new(defaults());
        let devices = vec![dev(1, "Mac", true, vec![disp(1, 0, 1440, 900)]), dev(3, "Linux", false, vec![disp(1, 0, 1280, 800)])];
        e.sync(devices, &LayoutDoc::default(), None, defaults());
        // The unplaced device sits to the right, and Apply stores that placement.
        assert_eq!(e.tiles()[1].rect.x, 1440.0);
        let doc = e.draft();
        assert_eq!(doc.placement(&fp(3)), Some((1440, 0)));
        assert!(!e.drag_preview(1, -5.0, 3.0).is_empty(), "near an edge: guides appear");
        assert!(e.drag_preview(1, 5000.0, 5000.0).is_empty());
    }

    #[test]
    fn unapplied_changes_survive_unrelated_updates_but_a_newer_applied_layout_wins() {
        let mut e = editor();
        e.drop_device(1, 0.0, 40.0);
        let devices = vec![dev(1, "Mac", true, vec![disp(1, 0, 1440, 900)]), dev(2, "PC", false, vec![disp(1, 0, 1920, 1080)])];
        let mut same = LayoutDoc::default();
        same.set_placement(fp(1), 0, 0);
        same.set_placement(fp(2), 1440, 0);
        same.revision = 1;
        e.sync(devices.clone(), &same, None, defaults());
        assert!(e.dirty(), "an unrelated snapshot must not discard the user's experiment");
        let mut newer = same.clone();
        newer.revision = 2;
        e.sync(devices, &newer, None, defaults());
        assert!(!e.dirty(), "a layout applied elsewhere replaces the draft");
    }
}
