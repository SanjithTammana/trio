//! Routing map: items | branches | components, with routed wires between neighbouring columns
//! and a time strip underneath. Items link to the branches that implement them; branches link
//! to the parts of the codebase they change since their merge-base. A component two or more open
//! branches change is a hot spot (likely conflict) and carries the one accent colour.
//!
//! Layout is O(V + E log E): fixed row height, one barycentre pass per column (the classic
//! one-sided crossing-minimization heuristic), no force simulation. The model is rebuilt only
//! when the snapshot revision or the scope changes, not per frame.

use crate::app::{Open, Panel, Pick, TrioApp};
use crate::model::{components, iso, lanes, ms, name_of, time_label, Component, Node, Snapshot};
use crate::paint::{self, Hit};
use crate::theme::{self, fade, Tokens};
use crate::glyph;
use eframe::egui::{self, epaint::CubicBezierShape, pos2, vec2, Color32, Id, Rect, Stroke, StrokeKind, UiBuilder};
use std::collections::{HashMap, HashSet};

const ROW: f32 = 30.0;
const GAP: f32 = 56.0;
const ACTIVE_HOURS: i64 = 36;
const MAX_COMPONENTS: usize = 28;
const PRIORITY: [&str; 6] = ["changes", "blocked", "review", "claimed", "open", "pass"];
const HEADER: f32 = 41.0;

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
    branches.sort_by(|&a, &b| all[b].last.cmp(&all[a].last));
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
    let mins = [230.0, 210.0, 170.0];
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
    let mut x = bar.left() + 16.0;
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
    // Legend, right-aligned.
    let legend: Vec<(&str, &str, bool)> = vec![("open", "open", false), ("claimed", "claimed", false), ("review", "review", false), ("changes", "changes", false), ("pass", "pass", false), ("branch", "branch", false), ("component", "hot spot", true)];
    let lg: Vec<_> = legend.iter().map(|(_, l, _)| paint::line(ui, l, theme::sans(12.5), t.ink3, 200.0)).collect();
    let lw: f32 = lg.iter().map(|g| 15.0 + g.size().x).sum::<f32>() + 12.0 * (lg.len() as f32 - 1.0);
    if bar.right() - 16.0 - lw > x + 16.0 {
        let mut lx = bar.right() - 16.0 - lw;
        for ((state, _, hot), g) in legend.iter().zip(lg) {
            glyph::state(&p, Rect::from_min_size(pos2(lx, bar.center().y - 5.0), vec2(10.0, 10.0)), state, *hot, t.ink3, t.accent);
            p.galley(pos2(lx + 15.0, bar.center().y - g.size().y / 2.0), g.clone(), t.ink3);
            lx += 15.0 + g.size().x + 12.0;
        }
    }

    // ---------- strip ----------
    let strip_nodes: Vec<&Node> = m.items.iter().chain(m.branches.iter()).map(|&i| &all[i]).collect();
    let strip_h = strip(ui, area, &strip_nodes, &lit, snap, t, day);

    // ---------- columns ----------
    let body = Rect::from_min_max(pos2(area.left(), bar.bottom()), pos2(area.right(), area.bottom() - strip_h));
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
        let width = (body.width() - 32.0).max(780.0);
        let [w1, w2, w3] = columns(width);
        let x1 = full.left() + 16.0;
        let x2 = x1 + w1 + GAP;
        let x3 = x2 + w2 + GAP;
        let rows = m.items.len().max(m.branches.len()).max(m.comps.len()).max(1) as f32;
        let height = HEADER + rows * ROW + 24.0;
        let (content, _) = ui.allocate_exact_size(vec2(width + 32.0, height), egui::Sense::hover());
        let top = content.top() + HEADER;
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
        hp.rect_filled(Rect::from_min_size(pos2(content.left(), hy), vec2(content.width(), HEADER - 6.0)), 0.0, t.bg);
        let header = |x: f32, w: f32, label: &str, n: String, hot: usize| {
            let mut job = paint::caps(label, 11.0, t.ink3);
            job.append(&n, 6.0, paint::fmt(theme::mono(11.0), t.ink3));
            if hot > 0 {
                job.append(&format!("{hot} hot"), 8.0, paint::fmt(theme::sans(11.0), t.accent));
            }
            paint::elide(&mut job, w);
            let g = paint::job(ui, job);
            hp.galley(pos2(x, hy + 12.0), g, t.ink3);
            hp.line_segment([pos2(x, hy + HEADER - 6.0), pos2(x + w, hy + HEADER - 6.0)], Stroke::new(1.0, t.line));
        };
        header(x1, w1, "Items", m.items.len().to_string(), 0);
        header(x1 + w1, GAP, "", String::new(), 0);
        header(x2, w2, "Branches", m.branches.len().to_string(), 0);
        header(x2 + w2, GAP, "", String::new(), 0);
        let cn = if m.hidden_comps > 0 { format!("{}+{}", m.comps.len(), m.hidden_comps) } else { m.comps.len().to_string() };
        header(x3, w3, "Components", cn, m.hot);
    });
    if app.map.hover != hover_now {
        app.map.hover = hover_now;
        ui.ctx().request_repaint();
    }
    let _ = StrokeKind::Inside;
    open
}

fn app_reduce(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp::<bool>(Id::unique("reduce-motion"))).unwrap_or(false)
}

/// Every recorded step of the visible work, one row per participant, on a shared time axis.
/// The focused node's steps stand out, so you can see when its work happened. Returns its height.
fn strip(ui: &mut egui::Ui, area: Rect, nodes: &[&Node], lit: &HashSet<String>, snap: &Snapshot, t: &Tokens, day: &str) -> f32 {
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
    let h = 8.0 + rows.len() as f32 * 14.0 + 16.0 + 6.0;
    let r = Rect::from_min_max(pos2(area.left(), area.bottom() - h), area.max);
    let p = ui.painter();
    p.rect_filled(r, 0.0, t.bg);
    p.line_segment([r.left_top(), r.right_top()], Stroke::new(1.0, t.line));
    let plot = Rect::from_min_max(pos2(r.left() + 16.0 + 70.0 + 10.0, r.top() + 8.0), pos2(r.right() - 16.0, r.top() + 8.0 + rows.len() as f32 * 14.0));
    for (i, who) in rows.iter().enumerate() {
        let y = plot.top() + i as f32 * 14.0;
        glyph::who(p, Rect::from_min_size(pos2(r.left() + 16.0, y + 2.0), vec2(10.0, 10.0)), who, me, false, t.ink3);
        let g = paint::line(ui, &name_of(who, me), theme::sans(11.0), t.ink3, 55.0);
        p.galley(pos2(r.left() + 31.0, y + 7.0 - g.size().y / 2.0), g, t.ink3);
        p.line_segment([pos2(plot.left(), y + 7.0), pos2(plot.right(), y + 7.0)], Stroke::new(1.0, t.line));
    }
    let x = |ms: i64| if hi == lo { plot.center().x } else { plot.left() + (ms - lo) as f32 / (hi - lo) as f32 * plot.width() };
    ticks.sort_by_key(|k| k.2);
    for (ms, row, on) in ticks {
        let xx = x(ms);
        let y = plot.top() + row as f32 * 14.0;
        p.line_segment([pos2(xx, y + 2.0), pos2(xx, y + 12.0)], Stroke::new(2.0, if on { t.ink } else { t.line2 }));
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
        assert!(a >= 230.0 && b >= 210.0 && c >= 170.0);
        let [a, b, _] = columns(2000.0);
        assert!((a / b - 1.25).abs() < 0.01);
    }
}
