//! Routing map: items | branches | components, with routed wires between neighbouring columns
//! and a time strip underneath. Items link to the branches that implement them; branches link
//! to the parts of the codebase they change since their merge-base. A component two or more open
//! branches change is a hot spot (likely conflict) and carries the one accent colour.
//!
//! Layout is O(V + E log E): fixed row height, one barycentre pass per column (the classic
//! one-sided crossing-minimization heuristic), no force simulation. The model is rebuilt only
//! when the snapshot revision or the scope changes, not per frame.
//!
//! v3 puts lenses over the same data, as the web map does: Work (default: items in stage lanes,
//! cards waiting on you first), Code (the routing map) and Time (the activity strip at full
//! height). Explain mode (`?`) captions the lens, its columns and the strip in place.

use crate::app::{Open, Panel, Pick, TrioApp};
use crate::model::{components, iso, lanes, ms, name_of, time_label, Component, Node, Snapshot, WORK_LANES};
use crate::paint::{self, Hit};
use crate::theme::{self, fade, Tokens};
use crate::glyph;
use eframe::egui::{self, epaint::CubicBezierShape, pos2, vec2, Color32, CornerRadius, Id, Pos2, Rect, Stroke, StrokeKind, UiBuilder, WidgetInfo};
use std::collections::{HashMap, HashSet};

const ROW: f32 = 30.0;
const GAP: f32 = 56.0;
/// Column minimum widths: items, branches, components.
const MINS: [f32; 3] = [200.0, 180.0, 150.0];
const ACTIVE_HOURS: i64 = 36;
const MAX_COMPONENTS: usize = 28;
const PRIORITY: [&str; 6] = ["changes", "blocked", "review", "claimed", "open", "pass"];
const HEADER: f32 = 41.0;
/// Work lens: done cards stay 24 h in the In flight scope; lanes show 12 cards before "+N more".
const DONE_HOURS: i64 = 24;
const LANE_CAP: usize = 12;
const LANE_MIN: f32 = 180.0;
const LANE_GAP: f32 = 12.0;

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum Lens {
    #[default]
    Work,
    Code,
    Time,
}

const LENSES: [(Lens, &str, &str); 3] = [
    (Lens::Work, "Work", "What is happening, and who it is waiting on"),
    (Lens::Code, "Code", "Where in the code the work lands"),
    (Lens::Time, "Time", "When each step happened, per participant"),
];

fn lens_explain(l: Lens) -> &'static str {
    match l {
        Lens::Work => "Work: every item by stage. A card waiting on you has a carmine edge and sorts first. Click a card for its story; nothing here costs a wake.",
        Lens::Code => "Code: items (left) link to the branches that implement them (middle); branches link to the parts of the code they change since their merge-base (right). A part that two open branches change is a hot spot, a likely conflict.",
        Lens::Time => "Time: one row per participant, one tick per recorded step (a claim, commit, verdict or message). Hover a node in another lens to light its ticks.",
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    #[default]
    Active,
    All,
}

#[derive(Default)]
pub struct MapState {
    scope: Scope,
    hover: Option<String>,
    model: Option<(u64, String, Scope, Model)>,
    lens: Lens,
    /// None until toggled: explain is then on while the room is empty (where people learn).
    explain: Option<bool>,
    /// The catch-up on screen: its mark, when it began showing in a focused window, and when it
    /// was last drawn (a gap means another view or an unfocused window: the minute starts over).
    catchup_shown: Option<(String, f64, f64)>,
    /// The catch-up already marked seen by the timer (mark and position), so it posts once.
    catchup_sent: Option<(String, u64)>,
}

impl MapState {
    pub fn explain(&self, empty: bool) -> bool {
        self.explain.unwrap_or(empty)
    }
    pub fn toggle_explain(&mut self, empty: bool) {
        self.explain = Some(!self.explain(empty));
    }
}

#[derive(Clone, Default)]
pub struct Model {
    items: Vec<usize>,
    branches: Vec<usize>,
    comps: Vec<Component>,
    hidden_comps: usize,
    hot: usize,
    implemented: HashMap<String, Vec<String>>,
    implements_of: HashMap<String, Vec<String>>,
    merged: HashSet<String>,
}

fn priority(state: &str) -> i32 {
    PRIORITY.iter().position(|s| *s == state).map_or(-1, |i| i as i32)
}

pub fn build(snap: &Snapshot, scope: Scope) -> Model {
    let all = &snap.graph.items;
    let merged: HashSet<String> = snap.graph.merged.iter().cloned().collect();
    let mut implemented: HashMap<String, Vec<String>> = HashMap::new();
    let mut implements_of: HashMap<String, Vec<String>> = HashMap::new();
    for e in snap.graph.edges.iter().filter(|e| e.kind == "implements") {
        implemented.entry(e.to.clone()).or_default().push(e.from.clone());
        implements_of.entry(e.from.clone()).or_default().push(e.to.clone());
    }
    let newest = all.iter().map(|n| ms(&n.last)).max().unwrap_or(0);
    let cutoff = newest - ACTIVE_HOURS * 3_600_000;

    let mut branches: Vec<usize> = (0..all.len()).filter(|&i| all[i].kind == "branch" && (scope == Scope::All || !merged.contains(&all[i].id))).collect();
    branches.sort_by(|&a, &b| all[b].last.cmp(&all[a].last).then_with(|| all[a].id.cmp(&all[b].id)));
    let b_index: HashMap<&str, usize> = branches.iter().enumerate().map(|(k, &i)| (all[i].id.as_str(), k)).collect();

    let mut items: Vec<usize> = (0..all.len())
        .filter(|&i| {
            let n = &all[i];
            if n.kind != "item" {
                return false;
            }
            if scope == Scope::All {
                return true;
            }
            if implemented.get(&n.id).is_some_and(|bs| bs.iter().any(|b| b_index.contains_key(b.as_str()))) {
                return true;
            }
            ms(&n.last) >= cutoff && n.state != "pass" && (n.touches > 1 || n.state != "open")
        })
        .collect();
    // Barycentre of each item's branches, computed once (not inside the comparator).
    let bary: HashMap<usize, f64> = items
        .iter()
        .map(|&i| {
            let idx: Vec<f64> = implemented.get(&all[i].id).map(|bs| bs.iter().filter_map(|b| b_index.get(b.as_str()).map(|&k| k as f64)).collect()).unwrap_or_default();
            (i, if idx.is_empty() { f64::INFINITY } else { idx.iter().sum::<f64>() / idx.len() as f64 })
        })
        .collect();
    items.sort_by(|&a, &b| {
        let (ba, bb) = (bary[&a], bary[&b]);
        if ba.is_finite() && bb.is_finite() && ba != bb {
            return ba.total_cmp(&bb);
        }
        if ba.is_finite() != bb.is_finite() {
            return if ba.is_finite() { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater };
        }
        priority(&all[a].state).cmp(&priority(&all[b].state)).then_with(|| all[b].last.cmp(&all[a].last))
    });

    let open_branches: Vec<&str> = branches.iter().map(|&i| all[i].id.as_str()).filter(|b| !merged.contains(*b)).collect();
    let mut comps = components(&snap.graph.touches, &open_branches);
    let hot = comps.iter().filter(|c| c.hot).count();
    let total = comps.len();
    comps.sort_by(|a, b| b.hot.cmp(&a.hot).then(b.lines.cmp(&a.lines)));
    comps.truncate(MAX_COMPONENTS);
    let cb = |c: &Component| {
        let w = c.branches.iter().map(|b| b.lines as f64).sum::<f64>().max(1.0);
        c.branches.iter().map(|b| b_index.get(b.branch.as_str()).copied().unwrap_or(0) as f64 * b.lines as f64).sum::<f64>() / w
    };
    comps.sort_by(|a, b| cb(a).total_cmp(&cb(b)));
    Model { items, branches, hidden_comps: total - comps.len(), comps, hot, implemented, implements_of, merged }
}

/// What lights up around the focused node: one hop each way, two for items.
fn lit(focus: Option<&str>, m: &Model, snap: &Snapshot) -> HashSet<String> {
    let mut s = HashSet::new();
    let Some(focus) = focus else { return s };
    let touches = &snap.graph.touches;
    let add_branch = |s: &mut HashSet<String>, b: &str, with_items: bool| {
        s.insert(format!("b:{b}"));
        for t in touches.get(b).map(Vec::as_slice).unwrap_or(&[]) {
            s.insert(format!("c:{}", t.part));
        }
        if with_items {
            for i in m.implements_of.get(b).map(Vec::as_slice).unwrap_or(&[]) {
                s.insert(format!("i:{i}"));
            }
        }
    };
    let id = &focus[2..];
    if focus.starts_with("c:") {
        s.insert(focus.to_string());
        for x in m.comps.iter().find(|c| c.part == id).map(|c| c.branches.as_slice()).unwrap_or(&[]) {
            s.insert(format!("b:{}", x.branch));
            for i in m.implements_of.get(&x.branch).map(Vec::as_slice).unwrap_or(&[]) {
                s.insert(format!("i:{i}"));
            }
        }
    } else if focus.starts_with("b:") {
        add_branch(&mut s, id, true);
    } else {
        s.insert(focus.to_string());
        for b in m.implemented.get(id).cloned().unwrap_or_default() {
            add_branch(&mut s, &b, false);
        }
    }
    s
}

/// Column widths for `minmax(230px,1.25fr) 56px minmax(210px,1fr) 56px minmax(170px,.85fr)`.
fn columns(width: f32) -> [f32; 3] {
    let mins = MINS;
    let fr = [1.25, 1.0, 0.85];
    let free = (width - 2.0 * GAP).max(mins.iter().sum());
    let mut fixed = [false; 3];
    loop {
        let rest: f32 = free - (0..3).filter(|&i| fixed[i]).map(|i| mins[i]).sum::<f32>();
        let weight: f32 = (0..3).filter(|&i| !fixed[i]).map(|i| fr[i]).sum();
        let w: Vec<f32> = (0..3).map(|i| if fixed[i] { mins[i] } else { rest * fr[i] / weight }).collect();
        if let Some(i) = (0..3).find(|&i| !fixed[i] && w[i] < mins[i]) {
            fixed[i] = true;
            continue;
        }
        return [w[0], w[1], w[2]];
    }
}

fn wire(p: &egui::Painter, x: f32, y1: f32, y2: f32, stroke: Stroke) {
    let m = GAP / 2.0;
    p.add(CubicBezierShape::from_points_stroke([pos2(x, y1), pos2(x + m, y1), pos2(x + m, y2), pos2(x + GAP, y2)], false, Color32::TRANSPARENT, stroke));
}

pub fn show(ui: &mut egui::Ui, app: &mut TrioApp, snap: &Snapshot, t: &Tokens, day: &str, selected: Option<&str>) -> Option<Open> {
    let me = snap.me.as_str();
    let area = ui.max_rect();
    let scope = app.map.scope;
    let stale = app.map.model.as_ref().is_none_or(|(rev, token, s, _)| *rev != snap.rev || *token != snap.token || *s != scope);
    if stale {
        app.map.model = Some((snap.rev, snap.token.clone(), scope, build(snap, scope)));
    }
    let m = app.map.model.as_ref().map(|(_, _, _, m)| m.clone()).expect("model");
    let all = &snap.graph.items;
    let focus = app.map.hover.clone().or(selected.map(str::to_string));
    let lit = lit(focus.as_deref(), &m, snap);
    let mut open = None;
    let mut hover_now: Option<String> = None;

    // ---------- bar ----------
    let bar = Rect::from_min_size(area.min, vec2(area.width(), 38.0));
    let p = ui.painter().clone();
    p.line_segment([bar.left_bottom(), bar.right_bottom()], Stroke::new(1.0, t.line));
    let lens = app.map.lens;
    let explain = app.map.explain(all.is_empty());
    let (seg, picked) = segmented(ui, &p, pos2(bar.left() + 16.0, bar.center().y - 14.0), lens, t);
    if let Some(l) = picked {
        app.map.lens = l;
    }
    let mut x = seg.right() + 12.0;
    for (s, label) in [(Scope::Active, "In flight"), (Scope::All, "Everything")] {
        let on = scope == s;
        let g = paint::line(ui, label, theme::sans(12.5), if on { t.ink } else { t.ink3 }, 200.0);
        let r = Rect::from_min_size(pos2(x, bar.center().y - 11.0), vec2(g.size().x + 18.0, 22.0));
        x = r.right() + 2.0;
        let hit = Hit::new(ui, r, Id::unique(("scope", label)), label);
        if on {
            p.rect_filled(r, 6.0, t.sunk);
        }
        p.galley(pos2(r.left() + 9.0, r.center().y - g.size().y / 2.0), g, t.ink);
        paint::focus_ring(&p, &hit.resp, t, 6.0);
        if hit.clicked() {
            app.map.scope = s;
        }
    }
    // Explain toggle: a pill with its key, like the web's `?` button.
    {
        let kg = paint::line(ui, "?", theme::mono(10.5), if explain { t.ink } else { t.ink3 }, 50.0);
        let lg = paint::line(ui, "Explain", theme::sans(12.0), if explain { t.ink } else { t.ink3 }, 200.0);
        let r = Rect::from_min_size(pos2(x + 10.0, bar.center().y - 11.0), vec2(20.0 + kg.size().x + 6.0 + lg.size().x, 22.0));
        x = r.right();
        let resp = ui.interact(r, Id::unique("explain-toggle"), egui::Sense::click());
        resp.widget_info(|| WidgetInfo::selected(egui::Role::Button, true, explain, "Explain (?)"));
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if explain {
            p.rect_filled(r, 11.0, t.sunk);
        }
        p.rect_stroke(r, 11.0, Stroke::new(1.0, if explain { t.line2 } else { t.line }), StrokeKind::Inside);
        p.galley(pos2(r.left() + 10.0, r.center().y - kg.size().y / 2.0), kg.clone(), t.ink3);
        p.galley(pos2(r.left() + 10.0 + kg.size().x + 6.0, r.center().y - lg.size().y / 2.0), lg, t.ink3);
        paint::focus_ring(&p, &resp, t, 11.0);
        let resp = resp.on_hover_text("Explain every column, glyph and the strip (?)");
        if resp.clicked() {
            app.map.toggle_explain(all.is_empty());
        }
    }
    // Legend, right-aligned: who is who on every lens, plus the lens's own marks.
    let agents: Vec<(String, String)> = crate::model::agent_names().into_iter().map(|a| (format!("@{a}"), crate::model::name_of(&a, &snap.me))).collect();
    let mut legend: Vec<(&str, &str, bool)> = vec![("@me", "you", false)];
    legend.extend(agents.iter().map(|(m, n)| (m.as_str(), n.as_str(), false)));
    match lens {
        Lens::Code => legend.extend([("open", "open", false), ("claimed", "claimed", false), ("review", "review", false), ("changes", "changes", false), ("pass", "pass", false), ("branch", "branch", false), ("component", "hot spot", true)]),
        Lens::Work => legend.push(("|you", "waiting on you", false)),
        Lens::Time => {}
    }
    let lg: Vec<_> = legend.iter().map(|(_, l, _)| paint::line(ui, l, theme::sans(12.5), t.ink3, 200.0)).collect();
    let lw: f32 = lg.iter().map(|g| 15.0 + g.size().x).sum::<f32>() + 12.0 * (lg.len() as f32 - 1.0);
    if bar.right() - 16.0 - lw > x + 16.0 {
        let mut lx = bar.right() - 16.0 - lw;
        for ((mark, _, hot), g) in legend.iter().zip(lg) {
            let r = Rect::from_min_size(pos2(lx, bar.center().y - 5.0), vec2(10.0, 10.0));
            if let Some(who) = mark.strip_prefix('@') {
                glyph::who(&p, r, if who == "me" { me } else { who }, me, false, t.ink3);
            } else if *mark == "|you" {
                p.rect_filled(Rect::from_center_size(r.center(), vec2(3.0, 12.0)), 2.0, t.accent);
            } else {
                glyph::state(&p, r, mark, *hot, t.ink3, t.accent);
            }
            p.galley(pos2(lx + 15.0, bar.center().y - g.size().y / 2.0), g.clone(), t.ink3);
            lx += 15.0 + g.size().x + 12.0;
        }
    }

    // ---------- explain caption ----------
    let mut below = bar.bottom();
    if explain {
        let g = paint::wrapped(ui, lens_explain(lens), italic(12.0), t.ink2, area.width() - 32.0);
        p.galley(pos2(area.left() + 16.0, below + 8.0), g.clone(), t.ink2);
        below += g.size().y + 16.0;
        p.line_segment([pos2(area.left(), below), pos2(area.right(), below)], Stroke::new(1.0, t.line));
    }

    // ---------- strip ----------
    let strip_nodes: Vec<&Node> = if lens == Lens::Code { m.items.iter().chain(m.branches.iter()).map(|&i| &all[i]).collect() } else { all.iter().collect() };
    if lens == Lens::Time {
        let r = Rect::from_min_max(pos2(area.left(), below), area.max);
        if strip(ui, r, &strip_nodes, &lit, snap, t, day, true, explain) == 0.0 {
            let g = paint::wrapped(ui, "No activity yet. Each claim, commit, verdict or message adds a tick on its author's row.", theme::sans(14.0), t.ink3, (r.width() - 32.0).min(640.0));
            ui.painter().galley(pos2(r.center().x - g.size().x / 2.0, r.top() + 48.0), g, t.ink3);
        }
        return finish(ui, app, hover_now, None);
    }
    let strip_h = strip(ui, area, &strip_nodes, &lit, snap, t, day, false, explain);
    let body = Rect::from_min_max(pos2(area.left(), below), pos2(area.right(), area.bottom() - strip_h));
    if lens == Lens::Work {
        let (used, caught) = match &snap.catchup {
            Some(c) => catchup(ui, body, app, snap, c, t, day),
            None => (0.0, None),
        };
        let body = Rect::from_min_max(pos2(body.left(), body.top() + used), body.max);
        let open = work(ui, body, app, snap, t, day, explain, selected, &mut hover_now).or(caught);
        return finish(ui, app, hover_now, open);
    }

    // ---------- columns ----------
    let mut cu = ui.new_child(UiBuilder::new().max_rect(body).id_salt("map-body"));
    cu.set_clip_rect(body);
    let empty = m.items.len() + m.branches.len() == 0;
    egui::ScrollArea::both().id_salt("map-scroll").auto_shrink([false, false]).show(&mut cu, |ui| {
        let full = ui.max_rect();
        if empty {
            let (r, _) = ui.allocate_exact_size(vec2(full.width(), 140.0), egui::Sense::hover());
            let g = paint::wrapped(ui, "No work in flight. Items appear when a message names one (P0a, XF29); branches when repo is set in room.toml.", theme::sans(14.0), t.ink3, (full.width() - 32.0).min(640.0));
            ui.painter().galley(pos2(r.center().x - g.size().x / 2.0, r.top() + 48.0), g, t.ink3);
            return;
        }
        // Size from the viewport: a both-ways scroll area's inner rect is not bounded by it.
        // Columns shrink to their minimums before the map scrolls sideways, so a side panel next to
        // a narrow window cuts labels with an ellipsis instead of clipping them at its edge.
        let width = (body.width() - 32.0).max(MINS.iter().sum::<f32>() + 2.0 * GAP);
        let [w1, w2, w3] = columns(width);
        let x1 = full.left() + 16.0;
        let x2 = x1 + w1 + GAP;
        let x3 = x2 + w2 + GAP;
        let rows = m.items.len().max(m.branches.len()).max(m.comps.len()).max(1) as f32;
        let hh = if explain { HEADER + 12.0 } else { HEADER };
        let height = hh + rows * ROW + 24.0;
        let (content, _) = ui.allocate_exact_size(vec2(width + 32.0, height), egui::Sense::hover());
        let top = content.top() + hh;
        let y_of = |k: usize| top + k as f32 * ROW + ROW / 2.0;
        let iy: HashMap<&str, f32> = m.items.iter().enumerate().map(|(k, &i)| (all[i].id.as_str(), y_of(k))).collect();
        let by: HashMap<&str, f32> = m.branches.iter().enumerate().map(|(k, &i)| (all[i].id.as_str(), y_of(k))).collect();
        let cy: HashMap<&str, f32> = m.comps.iter().enumerate().map(|(k, c)| (c.part.as_str(), y_of(k))).collect();
        let dim = focus.is_some();
        let ctx = ui.ctx().clone();
        let fade_of = |key: &str| -> f32 {
            let off = dim && !lit.contains(key);
            let k = ctx.animate_bool_with_time(Id::unique(("dim", key)), off, if app_reduce(&ctx) { 0.0 } else { 0.12 });
            1.0 - 0.7 * k
        };
        let p = ui.painter().clone();

        // Wires first, under the nodes.
        let mut left_wires: Vec<(&str, &str)> = Vec::new();
        for (b, its) in &m.implements_of {
            for i in its {
                if iy.contains_key(i.as_str()) && by.contains_key(b.as_str()) {
                    left_wires.push((b.as_str(), i.as_str()));
                }
            }
        }
        left_wires.sort();
        for (b, i) in left_wires {
            let on = lit.contains(&format!("i:{i}")) && lit.contains(&format!("b:{b}"));
            let c = if on { t.ink2 } else { fade(t.line2, if dim { 0.15 } else { 1.0 }) };
            wire(&p, x1 + w1, iy[i], by[b], Stroke::new(1.2, c));
        }
        for c in &m.comps {
            for x in &c.branches {
                let Some(&yb) = by.get(x.branch.as_str()) else { continue };
                let on = lit.contains(&format!("b:{}", x.branch)) && lit.contains(&format!("c:{}", c.part));
                let base = if c.hot { fade(t.accent, if on { 1.0 } else { 0.5 }) } else if on { t.ink2 } else { t.line2 };
                let col = if dim && !on { fade(base, 0.15) } else { base };
                let w = 1.0 + (2.5f32).min(((x.lines + 1) as f32).log10());
                wire(&p, x2 + w2, yb, cy[c.part.as_str()], Stroke::new(w, col));
            }
        }

        // Nodes.
        let mut node = |ui: &egui::Ui, key: String, rect: Rect, label: &str, tip: &str, draw: &dyn Fn(&egui::Painter, Rect, f32)| -> Option<(Rect, bool)> {
            let hit = Hit::new(ui, rect.expand2(vec2(6.0, 0.0)), Id::unique(("node", &key)), label);
            let alpha = fade_of(&key);
            let on = selected == Some(key.as_str());
            if hit.resp.hovered() || hit.resp.has_focus() {
                hover_now = Some(key.clone());
            }
            if hit.resp.hovered() || on {
                p.rect_filled(rect.expand2(vec2(6.0, 0.0)), 6.0, t.sunk);
            }
            draw(&p, rect, alpha);
            paint::focus_ring(&p, &hit.resp, t, 6.0);
            app.anchors.insert(key.clone(), rect.expand2(vec2(6.0, 0.0)));
            if !tip.is_empty() {
                hit.resp.clone().on_hover_text(tip.to_string());
            }
            hit.clicked().then(|| (rect.expand2(vec2(6.0, 0.0)), hit.pointer()))
        };
        for (k, &i) in m.items.iter().enumerate() {
            let n = &all[i];
            let rect = Rect::from_min_size(pos2(x1, top + k as f32 * ROW), vec2(w1, ROW));
            let lane = n.lane.clone();
            let draw = |p: &egui::Painter, r: Rect, a: f32| {
                glyph::state(p, Rect::from_min_size(pos2(r.left() + 1.0, r.center().y - 5.0), vec2(10.0, 10.0)), &n.state, false, fade(t.ink, a), fade(t.accent, a));
                let aux = Rect::from_min_size(pos2(r.right() - 10.0, r.center().y - 5.0), vec2(10.0, 10.0));
                glyph::who(p, aux, &lane, me, false, fade(t.ink3, a));
                let mut job = egui::text::LayoutJob::default();
                job.append(&n.id, 0.0, paint::fmt(theme::mono(12.5), fade(t.ink, a)));
                job.append(&n.summary, 8.0, paint::fmt(theme::sans(12.5), fade(t.ink3, a)));
                paint::elide(&mut job, r.width() - 20.0 - 18.0);
                let g = p.layout_job(job);
                p.galley(pos2(r.left() + 20.0, r.center().y - g.size().y / 2.0), g, t.ink);
            };
            if let Some((from, pointer)) = node(ui, format!("i:{}", n.id), rect, &n.id, &n.summary, &draw) {
                open = Some(Open { panel: Panel::Pick(Pick::Item(n.id.clone())), from, pointer });
            }
        }
        for (k, &i) in m.branches.iter().enumerate() {
            let n = &all[i];
            let rect = Rect::from_min_size(pos2(x2, top + k as f32 * ROW), vec2(w2, ROW));
            let is_merged = m.merged.contains(&n.id);
            let total: u32 = snap.graph.touches.get(&n.id).map(|ts| ts.iter().map(|t| t.lines).sum()).unwrap_or(0);
            let draw = |p: &egui::Painter, r: Rect, a: f32| {
                let st = if is_merged { "merged" } else { n.state.as_str() };
                glyph::state(p, Rect::from_min_size(pos2(r.left() + 1.0, r.center().y - 5.0), vec2(10.0, 10.0)), st, false, fade(t.ink, a), fade(t.accent, a));
                let who_r = Rect::from_min_size(pos2(r.right() - 10.0, r.center().y - 5.0), vec2(10.0, 10.0));
                glyph::who(p, who_r, &n.lane, me, false, fade(t.ink3, a));
                let mut aux_w = 10.0;
                if total > 0 {
                    let tg = p.layout_no_wrap(total.to_string(), theme::mono(11.0), fade(t.ink3, a));
                    aux_w += 6.0 + tg.size().x;
                    p.galley(pos2(who_r.left() - 6.0 - tg.size().x, r.center().y - tg.size().y / 2.0), tg, t.ink3);
                }
                let mut job = egui::text::LayoutJob::single_section(n.id.clone(), paint::fmt(theme::mono(12.5), fade(if is_merged { t.ink3 } else { t.ink }, a)));
                paint::elide(&mut job, r.width() - 20.0 - aux_w - 8.0);
                let g = p.layout_job(job);
                p.galley(pos2(r.left() + 20.0, r.center().y - g.size().y / 2.0), g, t.ink);
            };
            if let Some((from, pointer)) = node(ui, format!("b:{}", n.id), rect, &n.id, &n.summary, &draw) {
                open = Some(Open { panel: Panel::Pick(Pick::Branch(n.id.clone())), from, pointer });
            }
        }
        for (k, c) in m.comps.iter().enumerate() {
            let rect = Rect::from_min_size(pos2(x3, top + k as f32 * ROW), vec2(w3, ROW));
            let tip = format!("{} files, {} lines, {} branch{}", c.files, c.lines, c.branches.len(), if c.branches.len() > 1 { "es" } else { "" });
            let draw = |p: &egui::Painter, r: Rect, a: f32| {
                let ink = if c.hot { t.accent } else { t.ink };
                glyph::state(p, Rect::from_min_size(pos2(r.left() + 1.0, r.center().y - 5.0), vec2(10.0, 10.0)), "component", c.hot, fade(t.ink, a), fade(t.accent, a));
                let aux = if c.hot { format!("×{}  {}", c.branches.len(), c.lines) } else { c.lines.to_string() };
                let ag = p.layout_no_wrap(aux, theme::mono(11.0), fade(if c.hot { t.accent } else { t.ink3 }, a));
                p.galley(pos2(r.right() - ag.size().x, r.center().y - ag.size().y / 2.0), ag.clone(), t.ink3);
                let mut job = egui::text::LayoutJob::single_section(c.part.clone(), paint::fmt(theme::mono(12.5), fade(ink, a)));
                paint::elide(&mut job, r.width() - 20.0 - ag.size().x - 8.0);
                let g = p.layout_job(job);
                p.galley(pos2(r.left() + 20.0, r.center().y - g.size().y / 2.0), g, t.ink);
            };
            if let Some((from, pointer)) = node(ui, format!("c:{}", c.part), rect, &c.part, &tip, &draw) {
                open = Some(Open { panel: Panel::Pick(Pick::Component(c.part.clone())), from, pointer });
            }
        }
        if m.comps.is_empty() && !m.branches.is_empty() {
            let g = paint::wrapped(ui, "Set repo in room.toml to see what each branch changes.", theme::sans(12.5), t.ink3, w3);
            p.galley(pos2(x3, top + 4.0), g, t.ink3);
        }

        // Sticky column headers over the scrolled rows.
        let clip = ui.clip_rect();
        let hy = clip.top().max(content.top());
        let hp = p.with_clip_rect(clip);
        hp.rect_filled(Rect::from_min_size(pos2(content.left(), hy), vec2(content.width(), hh - 6.0)), 0.0, t.bg);
        let header = |x: f32, w: f32, label: &str, n: String, hot: usize| {
            let mut job = paint::caps(label, 11.0, t.ink3);
            job.append(&n, 6.0, paint::fmt(theme::mono(11.0), t.ink3));
            if hot > 0 {
                job.append(&format!("{hot} hot"), 8.0, paint::fmt(theme::sans(11.0), t.accent));
            }
            paint::elide(&mut job, w);
            let g = paint::job(ui, job);
            hp.galley(pos2(x, hy + 12.0), g, t.ink3);
            hp.line_segment([pos2(x, hy + hh - 6.0), pos2(x + w, hy + hh - 6.0)], Stroke::new(1.0, t.line));
        };
        header(x1, w1, "Items", m.items.len().to_string(), 0);
        header(x1 + w1, GAP, "", String::new(), 0);
        header(x2, w2, "Branches", m.branches.len().to_string(), 0);
        header(x2 + w2, GAP, "", String::new(), 0);
        let cn = if m.hidden_comps > 0 { format!("{}+{}", m.comps.len(), m.hidden_comps) } else { m.comps.len().to_string() };
        header(x3, w3, "Components", cn, m.hot);
        if explain {
            // Captions sit in the header band's spare height, one line each.
            for (x, w, text) in [(x1, w1, "Work named in chat (P0a, XF29)"), (x2, w2, "Git branches whose commits name an item"), (x3, w3, "Folders each branch changes, in lines")] {
                let g = paint::line(ui, text, italic(11.0), t.ink3, w);
                hp.galley(pos2(x, hy + 28.0), g, t.ink3);
            }
        }
    });
    finish(ui, app, hover_now, open)
}

/// Store this frame's hover (so the strip and dimming follow it) and hand back what to open.
fn finish(ui: &egui::Ui, app: &mut TrioApp, hover_now: Option<String>, open: Option<Open>) -> Option<Open> {
    if app.map.hover != hover_now {
        app.map.hover = hover_now;
        ui.ctx().request_repaint();
    }
    open
}

fn italic(size: f32) -> egui::FontId {
    // egui has no italic face by default; the caption reads as one by size and colour alone.
    theme::sans(size)
}

/// Lens switch, ported from Astryx's SegmentedControl (MIT, Copyright (c) 2026 Meta Platforms, Inc.;
/// see THIRD_PARTY_NOTICES.md):
/// a radio group whose selection indicator slides between segments, Arrow keys / Home / End move
/// the selection while a segment has focus, and the ends wrap.
fn segmented(ui: &mut egui::Ui, p: &egui::Painter, at: Pos2, lens: Lens, t: &Tokens) -> (Rect, Option<Lens>) {
    let gal: Vec<_> = LENSES.iter().map(|(_, l, _)| paint::line(ui, l, theme::sans(12.5), t.ink, 200.0)).collect();
    let widths: Vec<f32> = gal.iter().map(|g| g.size().x + 20.0).collect();
    let total = widths.iter().sum::<f32>() + 2.0 * (LENSES.len() as f32 - 1.0) + 6.0;
    let outer = Rect::from_min_size(at, vec2(total, 28.0));
    p.rect_stroke(outer, 8.0, Stroke::new(1.0, t.line), StrokeKind::Inside);
    // The segments sit in a radiogroup node, as role="radiogroup" does on the web.
    let ui = &ui.new_child(UiBuilder::new().max_rect(outer).id_salt("lens-group"));
    ui.ctx().accesskit_node_builder(ui.unique_id(), |n| {
        n.set_role(egui::accesskit::Role::RadioGroup);
        n.set_label("Lens");
        n.set_bounds(egui::accesskit::Rect { x0: outer.left().into(), y0: outer.top().into(), x1: outer.right().into(), y1: outer.bottom().into() });
    });
    let mut rects = Vec::with_capacity(LENSES.len());
    let mut x = outer.left() + 3.0;
    for w in &widths {
        rects.push(Rect::from_min_size(pos2(x, outer.top() + 3.0), vec2(*w, 22.0)));
        x += w + 2.0;
    }
    let cur = LENSES.iter().position(|(l, _, _)| *l == lens).unwrap_or(0);
    // The indicator animates from the old segment's rect to the new one (instant under reduced motion).
    let ctx = ui.ctx();
    let dur = if app_reduce(ctx) { 0.0 } else { 0.22 };
    let left = ctx.animate_value_with_time(Id::unique("lens-pill-x"), rects[cur].left(), dur);
    let width = ctx.animate_value_with_time(Id::unique("lens-pill-w"), rects[cur].width(), dur);
    let pill = Rect::from_min_size(pos2(left, outer.top() + 3.0), vec2(width, 22.0));
    p.rect_filled(pill, 6.0, t.surface);
    p.rect_stroke(pill, 6.0, Stroke::new(1.0, t.line), StrokeKind::Outside);
    let ids: Vec<Id> = LENSES.iter().map(|(_, l, _)| Id::unique(("lens", *l))).collect();
    let mut out = None;
    let mut focused = None;
    for (i, ((l, label, hint), g)) in LENSES.iter().zip(gal).enumerate() {
        let r = rects[i];
        let on = i == cur;
        let resp = ui.interact(r, ids[i], egui::Sense::click());
        resp.widget_info(|| WidgetInfo::selected(egui::Role::RadioButton, true, on, *label));
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if resp.has_focus() {
            focused = Some(i);
            ui.memory_mut(|m| m.set_focus_lock_filter(ids[i], egui::EventFilter { horizontal_arrows: true, vertical_arrows: true, ..Default::default() }));
        }
        let color = if on || resp.hovered() { t.ink } else { t.ink3 };
        p.galley(pos2(r.left() + 10.0, r.center().y - g.size().y / 2.0), g, color);
        paint::focus_ring(p, &resp, t, 6.0);
        let resp = resp.on_hover_text(*hint);
        if resp.clicked() {
            out = Some(*l);
        }
    }
    if let Some(i) = focused {
        let n = LENSES.len();
        let to = ui.input(|inp| {
            if inp.key_pressed(egui::Key::ArrowRight) || inp.key_pressed(egui::Key::ArrowDown) {
                Some((i + 1) % n)
            } else if inp.key_pressed(egui::Key::ArrowLeft) || inp.key_pressed(egui::Key::ArrowUp) {
                Some((i + n - 1) % n)
            } else if inp.key_pressed(egui::Key::Home) {
                Some(0)
            } else if inp.key_pressed(egui::Key::End) {
                Some(n - 1)
            } else {
                None
            }
        });
        if let Some(to) = to {
            out = Some(LENSES[to].0);
            // Cancel egui's own arrow navigation for this pass, or it can move focus elsewhere.
            ui.memory_mut(|m| {
                m.move_focus(egui::FocusDirection::None);
                m.request_focus(ids[to]);
            });
        }
    }
    (outer, out)
}

/// Work lens: items in five stage lanes, cards waiting on you first. Returns what to open.
#[allow(clippy::too_many_arguments)]
fn work(ui: &mut egui::Ui, body: Rect, app: &mut TrioApp, snap: &Snapshot, t: &Tokens, day: &str, explain: bool, selected: Option<&str>, hover_now: &mut Option<String>) -> Option<Open> {
    let me = snap.me.as_str();
    let all = &snap.graph.items;
    let scope = app.map.scope;
    let lanes = work_lanes(all, me, scope);
    let total: usize = lanes.iter().map(Vec::len).sum();
    let mut cu = ui.new_child(UiBuilder::new().max_rect(body).id_salt("work-body"));
    cu.set_clip_rect(body);
    if total == 0 && scope == Scope::Active && all.iter().any(|n| n.kind == "item" && n.work.is_some()) {
        let g = paint::wrapped(&cu, "Nothing in flight right now. Everything shows the finished and older work.", theme::sans(14.0), t.ink3, (body.width() - 32.0).min(640.0));
        cu.painter().galley(pos2(body.center().x - g.size().x / 2.0, body.top() + 48.0), g, t.ink3);
        return None;
    }
    if total == 0 {
        let g = paint::wrapped(&cu, "No work yet. When a message names an item (P0a, XF29), it shows here in its stage: asked, in progress, in review, changes or done.", theme::sans(14.0), t.ink3, (body.width() - 32.0).min(640.0));
        cu.painter().galley(pos2(body.center().x - g.size().x / 2.0, body.top() + 48.0), g, t.ink3);
        return None;
    }
    let mut open = None;
    egui::ScrollArea::both().id_salt("work-scroll").auto_shrink([false, false]).show(&mut cu, |ui| {
        let full = ui.max_rect();
        let width = (body.width() - 32.0).max(5.0 * LANE_MIN + 4.0 * LANE_GAP);
        let lw = (width - 4.0 * LANE_GAP) / 5.0;
        let inner = lw - 20.0;
        // Captions share one height so the first cards line up across lanes.
        let caps: Vec<_> = WORK_LANES.iter().map(|(_, _, c)| explain.then(|| paint::wrapped(ui, c, italic(12.0), t.ink2, lw))).collect();
        let cap_h = caps.iter().flatten().map(|g| g.size().y).fold(0.0, f32::max);
        // Lay every card out once to size the scroll content, then draw.
        let cards: Vec<Vec<(&Node, std::sync::Arc<egui::Galley>, f32)>> = lanes
            .iter()
            .map(|list| {
                list.iter()
                    .take(if scope == Scope::All { usize::MAX } else { LANE_CAP })
                    .map(|n| {
                        // What it is leads; the id (XF29, P0a) is a tag in the meta row.
                        let mut job = egui::text::LayoutJob::default();
                        if n.summary.is_empty() {
                            job.append(&n.id, 0.0, paint::fmt(theme::mono(12.0), t.ink));
                        } else {
                            job.append(&n.summary, 0.0, paint::fmt(theme::sans(12.5), t.ink));
                        }
                        job.wrap.max_width = inner;
                        job.wrap.max_rows = 2;
                        job.wrap.overflow_character = Some('…');
                        let g = paint::job(ui, job);
                        let h = 8.0 + g.size().y + 4.0 + 16.0 + 8.0;
                        (*n, g, h)
                    })
                    .collect()
            })
            .collect();
        let head = 24.0 + if explain { cap_h + 8.0 } else { 0.0 };
        let lane_h = |k: usize| cards[k].iter().map(|c| c.2 + 8.0).sum::<f32>() + if lanes[k].len() > cards[k].len() { 22.0 } else { 0.0 };
        let height = 14.0 + head + (0..5).map(lane_h).fold(0.0, f32::max) + 14.0;
        let (content, _) = ui.allocate_exact_size(vec2(width + 32.0, height.max(full.height())), egui::Sense::hover());
        let p = ui.painter().clone();
        for (k, (lane, title, _)) in WORK_LANES.iter().enumerate() {
            let x = content.left() + 16.0 + k as f32 * (lw + LANE_GAP);
            let mut y = content.top() + 14.0;
            let label = if *lane == "done" && scope == Scope::Active { format!("{title} · 24h") } else { title.to_string() };
            let hg = paint::job(ui, paint::caps(&label, 11.0, t.ink3));
            p.galley(pos2(x, y), hg, t.ink3);
            let ng = paint::line(ui, &lanes[k].len().to_string(), theme::mono(11.0), t.ink3, 60.0);
            p.galley(pos2(x + lw - ng.size().x, y), ng, t.ink3);
            y += 24.0;
            if let Some(g) = &caps[k] {
                p.galley(pos2(x, y - 4.0), g.clone(), t.ink2);
                y += cap_h + 8.0;
            }
            for (n, g, h) in &cards[k] {
                let rect = Rect::from_min_size(pos2(x, y), vec2(lw, *h));
                y += h + 8.0;
                let w = n.work.as_ref().expect("work lane cards have work");
                let branch = n.kind == "branch";
                let key = format!("{}:{}", if branch { "b" } else { "i" }, n.id);
                let you = w.waiting_on.as_deref() == Some(me);
                let hit = Hit::new(ui, rect, Id::unique(("card", &n.id)), &format!("{} {}", n.id, n.summary));
                let lit = hit.hot() || selected == Some(key.as_str());
                if hit.hot() {
                    *hover_now = Some(key.clone());
                }
                p.rect_filled(rect, 8.0, if lit { t.sunk } else { t.surface });
                p.rect_stroke(rect, 8.0, Stroke::new(1.0, if lit { t.line2 } else { t.line }), StrokeKind::Inside);
                if you {
                    p.rect_filled(Rect::from_min_size(rect.min, vec2(3.0, rect.height())), CornerRadius { nw: 8, sw: 8, ne: 0, se: 0 }, t.accent);
                }
                p.galley(pos2(rect.left() + 10.0, rect.top() + 8.0), g.clone(), t.ink);
                // Meta row: who it waits on (or its state), and when it last moved.
                let my = rect.bottom() - 8.0 - 8.0;
                let mut mx = rect.left() + 10.0;
                if !n.summary.is_empty() {
                    let ig = paint::line(ui, &n.id, theme::mono(10.5), t.ink3, if branch { 140.0 } else { 90.0 });
                    let tag = Rect::from_min_size(pos2(mx, my - ig.size().y / 2.0 - 1.0), ig.size() + vec2(8.0, 2.0));
                    p.rect_stroke(tag, 4.0, Stroke::new(1.0, t.line), StrokeKind::Inside);
                    p.galley(tag.min + vec2(4.0, 1.0), ig, t.ink3);
                    mx = tag.right() + 6.0;
                }
                let mark = Rect::from_min_size(pos2(mx, my - 5.0), vec2(10.0, 10.0));
                let tg = paint::line(ui, &time_label(moved(n), day), theme::sans(11.5), t.ink3, 120.0);
                let meta_c = if you { t.accent } else { t.ink3 };
                let text = match (&w.waiting_on, w.lane.as_str()) {
                    (Some(who), _) => {
                        glyph::who(&p, mark, who, me, false, meta_c);
                        // Waiting on an agent that is out: say when it is back.
                        let held = snap.usage.get(who.as_str()).filter(|u| u.out()).map(|u| format!("{} out{}", name_of(who, me), u.until.as_deref().map(|b| format!(" \u{b7} back {}", crate::model::hhmm(b))).unwrap_or_default()));
                        if you { "waiting on you".to_string() } else { held.unwrap_or_else(|| name_of(who, me)) }
                    }
                    (None, "done") => {
                        glyph::state(&p, mark, "pass", false, t.ink3, t.accent);
                        "done".to_string()
                    }
                    (None, _) => {
                        glyph::state(&p, mark, &n.state, false, t.ink3, t.accent);
                        n.state.clone()
                    }
                };
                let mg = paint::line(ui, &text, theme::sans(11.5), meta_c, (rect.right() - 10.0 - tg.size().x - 8.0 - (mark.right() + 5.0)).max(20.0));
                p.galley(pos2(mark.right() + 5.0, my - mg.size().y / 2.0), mg, meta_c);
                p.galley(pos2(rect.right() - 10.0 - tg.size().x, my - tg.size().y / 2.0), tg, t.ink3);
                paint::focus_ring(&p, &hit.resp, t, 8.0);
                app.anchors.insert(key, rect);
                if !w.story.is_empty() {
                    hit.resp.clone().on_hover_text(w.story.clone());
                }
                if hit.clicked() {
                    let pick = if branch { Pick::Branch(n.id.clone()) } else { Pick::Item(n.id.clone()) };
                    open = Some(Open { panel: Panel::Pick(pick), from: rect, pointer: hit.pointer() });
                }
            }
            if lanes[k].len() > cards[k].len() {
                let g = paint::line(ui, &format!("+{} more in Everything", lanes[k].len() - cards[k].len()), theme::sans(12.0), t.ink3, lw);
                p.galley(pos2(x + 2.0, y), g, t.ink3);
            }
        }
    });
    open
}

/// "Since you left": what moved while the human was away, above the Work lanes. Returns the
/// height used and a picked item. Dismiss moves the room's seen mark to now.
fn catchup(ui: &mut egui::Ui, body: Rect, app: &mut TrioApp, snap: &Snapshot, c: &crate::model::Catchup, t: &Tokens, day: &str) -> (f32, Option<Open>) {
    const ROW: f32 = 24.0;
    let w = body.width() - 32.0;
    let cols = ((w / 300.0).floor() as usize).clamp(1, 3);
    let rows = c.moved.len().div_ceil(cols) + usize::from(c.more > 0);
    let h = 12.0 + 22.0 + if rows > 0 { 6.0 + rows as f32 * ROW } else { 0.0 } + 10.0;
    // Seen once it has been on screen for a minute in a focused window (Dismiss is the fast way).
    let (time, focused) = ui.ctx().input(|i| (i.time, i.focused));
    // A strip that changed (new rows: a new position) gets its own full minute.
    let strip = format!("{}|{}", c.since, c.upto);
    let shown = match &app.map.catchup_shown {
        Some((s, at, last)) if *s == strip && focused && time - *last < 2.0 => *at,
        _ => time,
    };
    app.map.catchup_shown = focused.then_some((strip, shown, time));
    let key = (c.since.clone(), c.upto);
    if focused && app.map.catchup_sent.as_ref() != Some(&key) {
        if time - shown >= 60.0 {
            app.map.catchup_sent = Some(key);
            let client = app.client().clone();
            let (token, upto) = (snap.token.clone(), c.upto);
            crate::net::background(move || client.seen(&token, upto), |_| ());
        } else {
            ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        }
    }
    let rect = Rect::from_min_size(pos2(body.left() + 16.0, body.top() + 12.0), vec2(w, h));
    let p = ui.painter().clone();
    p.rect_filled(rect, 8.0, t.sunk);
    p.rect_stroke(rect, 8.0, Stroke::new(1.0, t.line2), StrokeKind::Inside);
    let mut x = rect.left() + 12.0;
    let hy = rect.top() + 12.0 + 11.0;
    let since = paint::line(ui, &format!("Since {}", time_label(&c.since, day)), theme::bold(12.5), t.ink, 200.0);
    p.galley(pos2(x, hy - since.size().y / 2.0), since.clone(), t.ink);
    x += since.size().x + 10.0;
    // Dismiss, right-aligned; its label is its accessible name.
    let dg = paint::line(ui, "Dismiss", theme::sans(12.0), t.ink3, 80.0);
    let dr = Rect::from_center_size(pos2(rect.right() - 12.0 - (dg.size().x + 20.0) / 2.0, hy), vec2(dg.size().x + 20.0, 22.0));
    let facts = paint::line(ui, &c.facts(), theme::sans(12.5), t.ink2, (dr.left() - 10.0 - x).max(20.0));
    p.galley(pos2(x, hy - facts.size().y / 2.0), facts, t.ink2);
    let dh = Hit::new(ui, dr, Id::unique("catchup-dismiss"), "Dismiss");
    p.rect_stroke(dr, 11.0, Stroke::new(1.0, if dh.hot() { t.line2 } else { t.line }), StrokeKind::Inside);
    p.galley(dr.center() - dg.size() / 2.0, dg, if dh.hot() { t.ink } else { t.ink3 });
    paint::focus_ring(&p, &dh.resp, t, 11.0);
    if dh.clicked() {
        let client = app.client().clone();
        let (token, upto) = (snap.token.clone(), c.upto);
        crate::net::background(move || client.seen(&token, upto), |_| ());
    }
    let lane_title = |l: &str| WORK_LANES.iter().find(|x| x.0 == l).map_or(l.to_string(), |x| x.1.to_string());
    let cw = (w - 24.0 - (cols - 1) as f32 * 16.0) / cols as f32;
    let top = rect.top() + 12.0 + 22.0 + 6.0;
    let mut open = None;
    for (i, m) in c.moved.iter().enumerate() {
        let r = Rect::from_min_size(pos2(rect.left() + 12.0 + (i % cols) as f32 * (cw + 16.0), top + (i / cols) as f32 * ROW), vec2(cw, ROW - 2.0));
        let label = format!("{} {}", m.id, if m.title.is_empty() { &m.id } else { &m.title });
        let hit = Hit::new(ui, r, Id::unique(("catchup", &m.id)), &label);
        if hit.hot() {
            p.rect_filled(r, 6.0, t.surface);
        }
        let mut mx = r.left() + 4.0;
        if m.yours {
            p.rect_filled(Rect::from_min_size(r.min, vec2(3.0, r.height())), 2.0, t.accent);
            mx += 4.0;
        }
        let cy = r.center().y;
        let ig = paint::line(ui, &m.id, theme::mono(10.5), t.ink3, 90.0);
        let tag = Rect::from_min_size(pos2(mx, cy - ig.size().y / 2.0 - 1.0), ig.size() + vec2(8.0, 2.0));
        p.rect_stroke(tag, 4.0, Stroke::new(1.0, t.line), StrokeKind::Inside);
        p.galley(tag.min + vec2(4.0, 1.0), ig, t.ink3);
        let mv = match &m.from {
            Some(f) => format!("{} \u{2192} {}", lane_title(f), lane_title(&m.to)),
            None => format!("new \u{b7} {}", lane_title(&m.to)),
        };
        let mg = paint::line(ui, &mv, theme::sans(11.5), t.ink3, 160.0);
        p.galley(pos2(r.right() - 4.0 - mg.size().x, cy - mg.size().y / 2.0), mg.clone(), t.ink3);
        let tx = tag.right() + 8.0;
        let tg = paint::line(ui, if m.title.is_empty() { &m.id } else { &m.title }, theme::sans(12.5), t.ink, (r.right() - 4.0 - mg.size().x - 8.0 - tx).max(20.0));
        p.galley(pos2(tx, cy - tg.size().y / 2.0), tg, t.ink);
        paint::focus_ring(&p, &hit.resp, t, 6.0);
        if hit.clicked() {
            // An unlinked branch moves too: open it as the branch it is.
            let branch = snap.graph.items.iter().any(|n| n.id == m.id && n.kind == "branch");
            let pick = if branch { Pick::Branch(m.id.clone()) } else { Pick::Item(m.id.clone()) };
            open = Some(Open { panel: Panel::Pick(pick), from: r, pointer: hit.pointer() });
        }
    }
    if c.more > 0 {
        let g = paint::line(ui, &format!("+{} more", c.more), theme::sans(12.0), t.ink3, 120.0);
        p.galley(pos2(rect.left() + 16.0, top + (rows - 1) as f32 * ROW + 4.0), g, t.ink3);
    }
    (12.0 + h, open)
}

/// Items per Work lane, filtered by scope (old done and old asked items leave In flight) and
/// sorted with cards waiting on `me` first, then newest. O(n log n).
fn work_lanes<'a>(all: &'a [Node], me: &str, scope: Scope) -> Vec<Vec<&'a Node>> {
    // Age is measured from the newest item, as on the web (branches do not count).
    let newest = all.iter().filter(|n| n.work.is_some()).map(|n| ms(moved(n))).max().unwrap_or(0);
    let mut lanes: Vec<Vec<&Node>> = vec![Vec::new(); WORK_LANES.len()];
    // Items, plus branches no item names (an existing repo's own work), each with a Work reading.
    for n in all.iter() {
        let Some(w) = &n.work else { continue };
        let Some(k) = WORK_LANES.iter().position(|l| l.0 == w.lane) else { continue };
        let age = newest - ms(moved(n));
        if scope == Scope::Active && ((w.lane == "done" && age > DONE_HOURS * 3_600_000) || (w.lane == "asked" && age > ACTIVE_HOURS * 3_600_000)) {
            continue;
        }
        lanes[k].push(n);
    }
    let mine = |n: &Node| n.work.as_ref().and_then(|w| w.waiting_on.as_deref()) == Some(me);
    for l in &mut lanes {
        l.sort_by(|a, b| mine(b).cmp(&mine(a)).then_with(|| moved(b).cmp(moved(a))).then_with(|| a.id.cmp(&b.id)));
    }
    lanes
}

/// When a card last moved: its Work `last` (branch activity included), else the item's own.
fn moved(n: &Node) -> &str {
    n.work.as_ref().map(|w| w.last.as_str()).filter(|l| !l.is_empty()).unwrap_or(&n.last)
}

fn app_reduce(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(Id::unique("reduce-motion"))).unwrap_or(false)
}

/// Every recorded step of the visible work, one row per participant, on a shared time axis.
/// The focused node's steps stand out, so you can see when its work happened. Returns its height.
/// `tall` is the Time lens: 40 px rows from the top of `area` instead of 14 px rows at its foot.
#[allow(clippy::too_many_arguments)]
fn strip(ui: &mut egui::Ui, area: Rect, nodes: &[&Node], lit: &HashSet<String>, snap: &Snapshot, t: &Tokens, day: &str, tall: bool, explain: bool) -> f32 {
    let me = snap.me.as_str();
    let rows = lanes(me);
    let lane_of = |who: &str| rows.iter().position(|r| r == who).unwrap_or(0);
    let mut ticks: Vec<(i64, usize, bool)> = nodes
        .iter()
        .flat_map(|n| {
            let on = lit.contains(&format!("i:{}", n.id)) || lit.contains(&format!("b:{}", n.id));
            n.steps.iter().map(move |s| (ms(&s.at), lane_of(&s.who), on))
        })
        .filter(|k| k.0 != 0)
        .collect();
    if ticks.len() < 2 {
        return 0.0;
    }
    let lo = ticks.iter().map(|k| k.0).min().unwrap_or(0);
    let hi = ticks.iter().map(|k| k.0).max().unwrap_or(0);
    let rh = if tall { 40.0 } else { 14.0 };
    let note = explain.then(|| paint::line(ui, "Activity: one row per participant, one tick per recorded step. Hover or select a node to light its ticks.", italic(11.5), t.ink2, area.width() - 32.0));
    let note_h = note.as_ref().map_or(0.0, |g| g.size().y + 6.0);
    let h = 8.0 + note_h + rows.len() as f32 * rh + 16.0 + 6.0;
    let r = if tall { Rect::from_min_size(area.min, vec2(area.width(), h)) } else { Rect::from_min_max(pos2(area.left(), area.bottom() - h), area.max) };
    let p = ui.painter();
    p.rect_filled(r, 0.0, t.bg);
    if !tall {
        p.line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, t.line));
    }
    if let Some(g) = note {
        p.galley(pos2(r.left() + 16.0, r.top() + 6.0), g, t.ink2);
    }
    let top = r.top() + 8.0 + note_h;
    let plot = Rect::from_min_max(pos2(r.left() + 16.0 + 70.0 + 10.0, top), pos2(r.right() - 16.0, top + rows.len() as f32 * rh));
    for (i, who) in rows.iter().enumerate() {
        let y = plot.top() + i as f32 * rh;
        glyph::who(p, Rect::from_min_size(pos2(r.left() + 16.0, y + rh / 2.0 - 5.0), vec2(10.0, 10.0)), who, me, false, t.ink3);
        let g = paint::line(ui, &name_of(who, me), theme::sans(11.0), t.ink3, 55.0);
        p.galley(pos2(r.left() + 31.0, y + rh / 2.0 - g.size().y / 2.0), g, t.ink3);
        p.line_segment([pos2(plot.left(), y + rh / 2.0), pos2(plot.right(), y + rh / 2.0)], Stroke::new(1.0, t.line));
    }
    let x = |ms: i64| if hi == lo { plot.center().x } else { plot.left() + (ms - lo) as f32 / (hi - lo) as f32 * plot.width() };
    ticks.sort_by_key(|k| k.2);
    for (ms, row, on) in ticks {
        let xx = x(ms);
        let y = plot.top() + row as f32 * rh;
        p.line_segment([pos2(xx, y + 2.0), pos2(xx, y + rh - 2.0)], Stroke::new(2.0, if on { t.ink } else { t.line2 }));
    }
    let lg = paint::line(ui, &time_label(&iso(lo), day), theme::mono(10.0), t.ink3, 200.0);
    let hg = paint::line(ui, &time_label(&iso(hi), day), theme::mono(10.0), t.ink3, 200.0);
    p.galley(pos2(plot.left(), plot.bottom() + 2.0), lg, t.ink3);
    p.galley(pos2(plot.right() - hg.size().x, plot.bottom() + 2.0), hg, t.ink3);
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_honour_minimums_and_fractions() {
        let [a, b, c] = columns(780.0);
        assert!((a + b + c - (780.0 - 2.0 * GAP)).abs() < 0.01);
        assert!(a >= MINS[0] && b >= MINS[1] && c >= MINS[2]);
        let [a, b, _] = columns(2000.0);
        assert!((a / b - 1.25).abs() < 0.01);
    }
}
