//! Shell: island top bar (presence + wake budget, expands in place), Chat or Map, and one detail
//! panel that morphs out of whatever was clicked and collapses back into it.
//! Keys: / composer, c chat, g map, Esc close.

use crate::model::{lanes, name_of, time_label, today, Message, Snapshot};
use crate::net::Client;
use crate::paint::{self, Hit};
use crate::theme::{self, fade, mix, Tokens};
use crate::{chat, glyph, map, panel};
use eframe::egui::{self, pos2, vec2, Align2, Color32, Id, LayerId, Order, Rect, Stroke, StrokeKind, UiBuilder};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Morph timings, from the web UI's view transitions.
pub const MORPH: f64 = 0.26;
const FADE_OUT: f64 = 0.11;
const FADE_IN: f64 = 0.17;
const FADE_IN_DELAY: f64 = 0.05;

#[derive(Clone, Debug, PartialEq)]
pub enum Pick {
    Item(String),
    Branch(String),
    Component(String),
}

impl Pick {
    pub fn key(&self) -> String {
        match self {
            Pick::Item(id) => format!("i:{id}"),
            Pick::Branch(id) => format!("b:{id}"),
            Pick::Component(id) => format!("c:{id}"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Panel {
    Message(Message),
    Pick(Pick),
}

impl Panel {
    pub fn key(&self) -> String {
        match self {
            Panel::Message(m) => m.id.clone(),
            Panel::Pick(p) => p.key(),
        }
    }
}

/// A request from a view: open this, morphing from that rect if the pointer did it.
pub struct Open {
    pub panel: Panel,
    pub from: Rect,
    pub pointer: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Chat,
    Map,
}

/// A running morph between two rects.
#[derive(Clone, Copy)]
struct Morph {
    from: Rect,
    start: f64,
}

pub struct TrioApp {
    client: Client,
    visible: Arc<AtomicBool>,
    snap: Option<Arc<Snapshot>>,
    error: Option<String>,
    view: View,
    panel: Option<Panel>,
    opening: Option<Morph>,
    /// The panel being folded back into `target`, from the rect it last had.
    closing: Option<(Panel, Rect, Option<String>, f64)>,
    panel_rect: Rect,
    island: bool,
    island_morph: Option<(f64, bool)>,
    /// Where each openable element was drawn this frame (`data-morph` in the web UI).
    pub anchors: HashMap<String, Rect>,
    pub chat: chat::ChatState,
    pub map: map::MapState,
    pub details: Arc<Mutex<HashMap<String, Option<String>>>>,
    reduce: bool,
    title: String,
    dark: Option<bool>,
}

impl TrioApp {
    pub fn new(ctx: &egui::Context, mut client: Client) -> Self {
        let visible = Arc::new(AtomicBool::new(true));
        let c = ctx.clone();
        client.sync(visible.clone(), move || c.request_repaint());
        Self {
            client,
            visible,
            snap: None,
            error: None,
            view: View::Chat,
            panel: None,
            opening: None,
            closing: None,
            panel_rect: Rect::NOTHING,
            island: false,
            island_morph: None,
            anchors: HashMap::new(),
            chat: chat::ChatState::default(),
            map: map::MapState::default(),
            details: Arc::default(),
            reduce: theme::reduced_motion(),
            title: String::new(),
            dark: None,
        }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    fn animate(&self, pointer: bool) -> bool {
        pointer && !self.reduce
    }

    fn open(&mut self, o: Open, now: f64) {
        let first = self.panel.is_none();
        let animate = self.animate(o.pointer) && first;
        self.closing = None;
        self.opening = animate.then_some(Morph { from: o.from, start: now });
        self.panel = Some(o.panel);
    }

    fn close(&mut self, pointer: bool, now: f64) {
        let Some(p) = self.panel.take() else { return };
        self.opening = None;
        if self.animate(pointer) {
            let key = p.key();
            self.closing = Some((p, self.panel_rect, Some(key), now));
        }
    }

    fn toggle_island(&mut self, pointer: bool, now: f64) {
        self.island = !self.island;
        self.island_morph = self.animate(pointer).then_some((now, self.island));
    }

    fn keys(&mut self, ctx: &egui::Context, now: f64) {
        let typing = ctx.egui_wants_keyboard_input();
        let (esc, slash, g, c, ask) = ctx.input(|i| {
            let plain = !i.modifiers.ctrl && !i.modifiers.alt && !i.modifiers.command;
            let text = |s: &str| i.events.iter().any(|e| matches!(e, egui::Event::Text(t) if t == s));
            (i.key_pressed(egui::Key::Escape), plain && text("/"), plain && text("g"), plain && text("c"), plain && text("?"))
        });
        if esc {
            if self.island {
                self.island = false;
                self.island_morph = None;
            } else if self.panel.is_some() && !typing {
                self.close(false, now);
            }
            return;
        }
        if typing {
            return;
        }
        if slash {
            self.view = View::Chat;
            self.chat.focus = true;
        } else if g {
            self.view = View::Map;
        } else if c {
            self.view = View::Chat;
        } else if ask && self.view == View::Map {
            // `?` explains the map in place (captions on the lens, columns and strip).
            let empty = self.snap.as_ref().is_none_or(|s| s.graph.items.is_empty());
            self.map.toggle_explain(empty);
        }
    }
}

impl eframe::App for TrioApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // A minimized window drops the change stream so the server can sleep (like a hidden tab).
        let minimized = ctx.input(|i| i.viewport().minimized).unwrap_or(false);
        self.visible.store(!minimized, Ordering::Relaxed);
        let s = self.client.shared();
        let (snap, error) = (s.snap.clone(), s.error.clone());
        drop(s);
        // A lost or replaced server invalidates cached detail text, including a blank cached from a
        // fetch that failed while the peer was changing; reopening a message then fetches again.
        let token = |s: &Option<Arc<Snapshot>>| s.as_ref().map(|s| s.token.clone());
        if token(&snap) != token(&self.snap) {
            self.details.lock().expect("details").clear();
        }
        self.snap = snap;
        self.error = error;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let dark = ctx.global_style().visuals.dark_mode;
        let t = theme::tokens(&ctx);
        if self.dark != Some(dark) {
            theme::apply(&ctx, &t);
            self.dark = Some(dark);
        }
        let now = ctx.input(|i| i.time);
        // TRIO_FRAMES=<file>: record frame count and why the last repaint was asked for (idle checks).
        if let Some(path) = std::env::var_os("TRIO_FRAMES") {
            let cause = ctx.repaint_causes().iter().map(|c| c.to_string()).collect::<Vec<_>>().join("; ");
            let _ = std::fs::write(path, format!("{} {cause}", ctx.cumulative_pass_nr()));
        }
        ctx.data_mut(|d| d.insert_temp(Id::unique("reduce-motion"), self.reduce));
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0.0, t.bg);

        let Some(snap) = self.snap.clone() else {
            let p = ui.painter();
            paint::centered(p, full, "connecting…", theme::mono(14.0), t.ink3);
            if let Some(e) = &self.error {
                paint::centered(p, full.translate(vec2(0.0, 24.0)), e, theme::mono(12.0), t.ink3);
            }
            return;
        };
        if self.title != snap.room {
            self.title = snap.room.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!("{} · Trio", snap.room)));
        }
        self.keys(&ctx, now);
        let day = today();
        let narrow = full.width() < 720.0;

        // ---------- top bar ----------
        let top = Rect::from_min_size(full.min, vec2(full.width(), 52.0));
        ui.painter().line_segment([top.left_bottom(), top.right_bottom()], Stroke::new(1.0, t.line));
        let (tab_rect, tab_open) = self.tabs(ui, top, &t, narrow);
        if !narrow {
            let room_w = (tab_rect.left() - top.left() - 16.0 - 12.0).min(top.width() / 2.0 - 230.0).max(0.0);
            let mut job = egui::text::LayoutJob::default();
            job.append("trio / ", 0.0, paint::fmt(theme::mono(13.0), t.ink3));
            job.append(&snap.room, 0.0, paint::fmt(theme::mono(13.0), t.ink));
            paint::elide(&mut job, room_w);
            let g = paint::job(ui, job);
            ui.painter().galley(pos2(top.left() + 16.0, top.center().y - g.size().y / 2.0), g, t.ink);
        }
        if let Some(v) = tab_open {
            self.view = v;
        }

        // ---------- main + panel ----------
        let main = Rect::from_min_max(pos2(full.left(), top.bottom()), full.max);
        let panel_w = if narrow { main.width() } else { 440f32.min(main.width() * 0.42) };
        let slot = Rect::from_min_max(pos2(main.right() - panel_w, main.top()), main.max);
        let content = if self.panel.is_some() {
            if narrow { Rect::NOTHING } else { Rect::from_min_max(main.min, pos2(slot.left(), main.bottom())) }
        } else {
            main
        };
        self.anchors.clear();
        let mut open: Option<Open> = None;
        if content.is_positive() {
            let mut child = ui.new_child(UiBuilder::new().max_rect(content).id_salt("content"));
            child.set_clip_rect(content);
            let selected = match &self.panel {
                Some(Panel::Pick(p)) => Some(p.key()),
                _ => None,
            };
            open = match self.view {
                View::Chat => chat::show(&mut child, self, &snap, &t, &day),
                View::Map => map::show(&mut child, self, &snap, &t, &day, selected.as_deref()),
            };
        }
        if let Some(o) = open {
            self.open(o, now);
        }

        // The panel, on its own layer so a morph can grow over the content it came from.
        if self.panel.is_some() {
            self.panel_rect = slot;
            let (rect, fade_in) = match self.opening {
                Some(m) => {
                    let k = ((now - m.start) / MORPH) as f32;
                    if k >= 1.0 {
                        self.opening = None;
                    } else {
                        ctx.request_repaint();
                    }
                    let e = theme::spring(k);
                    (lerp_rect(m.from, slot, e), fade_progress(now - m.start, FADE_IN_DELAY, FADE_IN))
                }
                None => (slot, 1.0),
            };
            let layer = LayerId::new(Order::Middle, Id::unique("panel"));
            let mut pui = ui.new_child(UiBuilder::new().layer_id(layer).max_rect(rect).id_salt("panel"));
            pui.set_clip_rect(rect.intersect(main));
            let morphing = self.opening.is_some();
            let corner = if morphing { 6.0 * (1.0 - theme::spring(((now - self.opening.map_or(now, |m| m.start)) / MORPH) as f32)).max(0.0) } else { 0.0 };
            pui.painter().rect_filled(rect, corner, t.surface);
            if !narrow {
                pui.painter().line_segment([rect.left_top(), rect.left_bottom()], Stroke::new(1.0, t.line));
            }
            pui.multiply_opacity(fade_in);
            let panel_now = self.panel.clone().expect("panel");
            let close_rect = Rect::from_min_size(pos2(rect.right() - 14.0 - 26.0, rect.top() + 12.0), vec2(26.0, 26.0));
            let nav = panel::show(&mut pui, self, &snap, &panel_now, &t, &day, rect);
            let hit = Hit::new(&pui, close_rect, Id::unique("panel-close"), "Close (Esc)");
            if hit.hot() {
                pui.painter().rect_filled(close_rect, 6.0, t.sunk);
            }
            paint::centered(pui.painter(), close_rect, "×", theme::sans(18.0), if hit.hot() { t.ink } else { t.ink3 });
            if hit.clicked() {
                let pointer = hit.pointer();
                self.close(pointer, now);
            } else if let Some(nav) = nav {
                // Moving inside the panel swaps its content in place.
                self.panel = Some(nav);
            }
        }
        if let Some((p, from, target, start)) = self.closing.clone() {
            let k = ((now - start) / MORPH) as f32;
            let to = target.as_ref().and_then(|key| self.anchors.get(key).copied());
            match to {
                Some(to) if k < 1.0 => {
                    let rect = lerp_rect(from, to, theme::spring(k));
                    let layer = LayerId::new(Order::Middle, Id::unique("panel-closing"));
                    let painter = ctx.layer_painter(layer).with_clip_rect(main);
                    let alpha = 1.0 - fade_progress(now - start, 0.0, FADE_OUT);
                    painter.rect_filled(rect, 6.0 * theme::spring(k).min(1.0), fade(t.surface, alpha.max(0.0) * 0.9 + 0.1 * (1.0 - k)));
                    let _ = p;
                    ctx.request_repaint();
                }
                _ => self.closing = None,
            }
        }

        // ---------- island (drawn last: it expands over everything) ----------
        let island_center = if narrow { None } else { Some(top.center().x) };
        self.island_ui(ui, &snap, &t, &day, top, island_center, now);
    }
}

impl TrioApp {
    /// Chat | Map segmented control at the right of the top bar.
    fn tabs(&self, ui: &mut egui::Ui, top: Rect, t: &Tokens, narrow: bool) -> (Rect, Option<View>) {
        let items = [(View::Chat, "Chat", "c"), (View::Map, "Map", "g")];
        let gal: Vec<_> = items
            .iter()
            .map(|(_, l, k)| (paint::line(ui, l, theme::sans(13.0), t.ink, 200.0), (!narrow).then(|| paint::line(ui, k, theme::mono(10.5), t.ink3, 50.0))))
            .collect();
        let widths: Vec<f32> = gal.iter().map(|(l, k)| 20.0 + l.size().x + k.as_ref().map_or(0.0, |k| 6.0 + k.size().x)).collect();
        let total = widths.iter().sum::<f32>() + 2.0 * (items.len() as f32 - 1.0) + 6.0;
        let outer = Rect::from_min_size(pos2(top.right() - 16.0 - total, top.center().y - 14.0), vec2(total, 28.0));
        let p = ui.painter();
        p.rect_stroke(outer, 8.0, Stroke::new(1.0, t.line), StrokeKind::Inside);
        let mut x = outer.left() + 3.0;
        let mut out = None;
        for (i, ((view, label, _), (_lg, kg))) in items.iter().zip(gal).enumerate() {
            let r = Rect::from_min_size(pos2(x, outer.top() + 3.0), vec2(widths[i], 22.0));
            x += widths[i] + 2.0;
            let on = self.view == *view;
            let hit = Hit::new(ui, r, Id::unique(("tab", i)), label);
            if on {
                p.rect_filled(r, 6.0, t.surface);
                p.rect_stroke(r, 6.0, Stroke::new(1.0, t.line), StrokeKind::Outside);
            }
            let color = if on { t.ink } else { t.ink3 };
            let lg = paint::line(ui, label, theme::sans(13.0), color, 200.0);
            p.galley(pos2(r.left() + 10.0, r.center().y - lg.size().y / 2.0), lg.clone(), color);
            if let Some(kg) = kg {
                p.galley(pos2(r.left() + 10.0 + lg.size().x + 6.0, r.center().y - kg.size().y / 2.0 + 1.0), kg, t.ink3);
            }
            paint::focus_ring(p, &hit.resp, t, 6.0);
            if hit.clicked() {
                out = Some(*view);
            }
        }
        (outer, out)
    }

    #[allow(clippy::too_many_arguments)]
    fn island_ui(&mut self, ui: &mut egui::Ui, snap: &Snapshot, t: &Tokens, day: &str, top: Rect, center_x: Option<f32>, now: f64) {
        let me = snap.me.as_str();
        let ink_on = |p: f32| mix(t.bg, p, t.ink);
        let status = |w: &str| -> (bool, String) {
            let a = snap.agents.get(w).cloned().unwrap_or_default();
            let now = snap.graph.now.get(w);
            let text = now.map(|n| n.text.clone()).unwrap_or_default();
            if a.running || now.is_some_and(|n| n.running) {
                return (true, if text.is_empty() { "working…".into() } else { text });
            }
            if let Some(app) = &a.attached {
                let app = match app.as_str() {
                    "codex" => "Codex",
                    "claude" => "Claude Code",
                    other => other,
                };
                return (false, format!("attached in {app}{}", if text.is_empty() { String::new() } else { format!(" · {text}") }));
            }
            if w != me && !a.enabled {
                return (false, if text.is_empty() { "not in the room".into() } else { text });
            }
            (false, if text.is_empty() { "idle".into() } else { text })
        };
        let narrow = center_x.is_none();
        let hop = if snap.paused { "paused".to_string() } else { format!("{}/{}", snap.hop, snap.budget) };
        let lane = lanes(me);

        // Closed capsule geometry.
        let names: Vec<_> = lane.iter().map(|w| paint::line(ui, &name_of(w, me), theme::sans(12.5), t.bg, 200.0)).collect();
        let hop_g = paint::line(ui, &hop, theme::mono(12.5), t.bg, 200.0);
        let parts: f32 = names.iter().map(|g| 10.0 + if narrow { 0.0 } else { 5.0 + g.size().x }).sum::<f32>() + 12.0 * 3.0 + 1.0 + 12.0 + hop_g.size().x;
        let w = parts + 28.0;
        let cx = center_x.unwrap_or(top.left() + 16.0 + w / 2.0);
        let capsule = Rect::from_center_size(pos2(cx, top.center().y), vec2(w, 32.0));

        // The open card's rows are laid out here first: a status that wraps makes its row taller,
        // and the card must grow with it or the budget row and buttons fall outside it.
        let open_w = 440f32.min(top.width() * 0.92);
        let rows: Vec<String> = lane
            .iter()
            .map(|wn| {
                let (_, text) = status(wn);
                let at = snap.graph.now.get(wn.as_str()).map(|n| n.at.clone()).unwrap_or_default();
                if at.is_empty() { text } else { format!("{text} · {}", time_label(&at, day)) }
            })
            .collect();
        let row_w = open_w - 32.0 - 88.0;
        let rows_h: f32 = rows.iter().map(|r| paint::wrapped(ui, r, theme::sans(12.5), t.bg, row_w).size().y.max(20.0) + 8.0).sum();
        let open_h = 12.0 + rows_h + 8.0 + 26.0 + 12.0;
        let open_x = if narrow { (top.left() + 16.0).min(top.right() - open_w - 8.0) } else { cx - open_w / 2.0 };
        let open_rect = Rect::from_min_size(pos2(open_x, top.center().y - 16.0 - 2.0), vec2(open_w, open_h));

        let (k, morph_to_open) = match self.island_morph {
            Some((start, to_open)) => {
                let k = ((now - start) / MORPH) as f32;
                if k >= 1.0 {
                    self.island_morph = None;
                    (1.0, to_open)
                } else {
                    ui.ctx().request_repaint();
                    (k, to_open)
                }
            }
            None => (1.0, self.island),
        };
        let e = theme::spring(k);
        let (from, to) = if morph_to_open { (capsule, open_rect) } else { (open_rect, capsule) };
        let rect = lerp_rect(from, to, e);
        let radius = if morph_to_open { 16.0 + (18.0 - 16.0) * e } else { 18.0 + (16.0 - 18.0) * e };
        let layer = LayerId::new(Order::Foreground, Id::unique("island"));
        let mut iu = ui.new_child(UiBuilder::new().layer_id(layer).max_rect(rect).id_salt("island"));
        let p = iu.painter().clone();
        if self.island || self.island_morph.is_some() {
            let shadow = egui::epaint::Shadow { offset: [0, 8], blur: 24, spread: 0, color: fade(Color32::BLACK, if self.dark == Some(true) { 0.6 } else { 0.18 }) };
            p.add(shadow.as_shape(rect, paint::radius(radius)));
        }
        p.rect_filled(rect, paint::radius(radius), t.ink);
        let content_alpha = if self.island_morph.is_some() { fade_progress(k as f64 * MORPH, FADE_IN_DELAY, FADE_IN) } else { 1.0 };
        let showing_open = if self.island_morph.is_some() { morph_to_open } else { self.island };

        if !showing_open {
            let hit = Hit::new(&iu, capsule, Id::unique("island-capsule"), "Room status");
            let mut x = capsule.left() + 14.0;
            for (i, wn) in lane.iter().enumerate() {
                let (live, text) = status(wn);
                let mut c = if live { t.bg } else { ink_on(0.55) };
                if live && !self.reduce {
                    let phase = (now % 1.6) / 1.6;
                    let a = 1.0 - 0.65 * (0.5 - 0.5 * (phase * std::f64::consts::TAU).cos()) as f32;
                    c = fade(c, a);
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
                }
                let gr = Rect::from_min_size(pos2(x, capsule.center().y - 5.0), vec2(10.0, 10.0));
                glyph::who(&p, gr, wn, me, false, fade(c, content_alpha));
                x += 10.0;
                if !narrow {
                    let g = paint::line(&iu, &name_of(wn, me), theme::sans(12.5), fade(if live { t.bg } else { ink_on(0.55) }, content_alpha), 200.0);
                    let r = Rect::from_min_size(pos2(x + 5.0, capsule.center().y - g.size().y / 2.0), g.size());
                    p.galley(r.min, g.clone(), t.bg);
                    x += 5.0 + g.size().x;
                }
                let tip_rect = Rect::from_x_y_ranges(gr.left()..=x, capsule.y_range());
                if iu.rect_contains_pointer(tip_rect) {
                    hit.resp.clone().on_hover_text(format!("{}: {text}", name_of(wn, me)));
                }
                x += if i < 2 { 12.0 } else { 0.0 };
            }
            x += 12.0;
            p.line_segment([pos2(x, capsule.center().y - 7.0), pos2(x, capsule.center().y + 7.0)], Stroke::new(1.0, fade(ink_on(0.25), content_alpha)));
            x += 13.0;
            let hop_c = if snap.paused { t.accent } else { ink_on(0.75) };
            p.galley(pos2(x, capsule.center().y - hop_g.size().y / 2.0), paint::line(&iu, &hop, theme::mono(12.5), fade(hop_c, content_alpha), 200.0), hop_c);
            paint::focus_ring(&p, &hit.resp, t, 16.0);
            if hit.clicked() {
                let pointer = hit.pointer();
                self.toggle_island(pointer, now);
            }
            return;
        }

        // Open: one row per participant, then the wake budget and Pause/Resume.
        let inner = rect.shrink2(vec2(16.0, 12.0));
        iu.multiply_opacity(content_alpha);
        let p = iu.painter().clone();
        let mut y = inner.top();
        for (wn, full) in lane.iter().zip(&rows) {
            glyph::who(&p, Rect::from_min_size(pos2(inner.left() + 2.0, y + 5.0), vec2(10.0, 10.0)), wn, me, false, t.bg);
            let ng = paint::line(&iu, &name_of(wn, me), theme::sans(12.5), t.bg, 58.0);
            p.galley(pos2(inner.left() + 22.0, y + 1.0), ng, t.bg);
            let sg = paint::wrapped(&iu, full, theme::sans(12.5), ink_on(0.72), inner.width() - 88.0);
            let h = sg.size().y.max(20.0);
            p.galley(pos2(inner.left() + 88.0, y + 1.0), sg, t.bg);
            y += h + 8.0;
        }
        p.line_segment([pos2(inner.left(), y), pos2(inner.right(), y)], Stroke::new(1.0, ink_on(0.18)));
        y += 8.0;
        let mono_text = if snap.paused { "agents will not wake each other".to_string() } else { format!("wakes {} of {}", snap.hop, snap.budget) };
        let mg = paint::line(&iu, &mono_text, theme::mono(12.0), ink_on(0.72), inner.width() - 150.0);
        p.galley(pos2(inner.left(), y + 13.0 - mg.size().y / 2.0), mg, t.bg);
        let mut bx = inner.right();
        let mut pill = |label: &str, on: bool, id: &str| -> (bool, bool) {
            let g = paint::line(&iu, label, theme::sans(13.0), if on { t.accent } else { t.bg }, 200.0);
            let r = Rect::from_min_size(pos2(bx - g.size().x - 22.0, y), vec2(g.size().x + 22.0, 26.0));
            bx = r.left() - 6.0;
            let hit = Hit::new(&iu, r, Id::unique(id), label);
            p.rect_stroke(r, 13.0, Stroke::new(1.0, if on { t.accent } else { ink_on(0.35) }), StrokeKind::Inside);
            p.galley(pos2(r.left() + 11.0, r.center().y - g.size().y / 2.0), g, t.bg);
            paint::focus_ring(&p, &hit.resp, t, 13.0);
            (hit.clicked(), hit.pointer())
        };
        let (done, done_ptr) = pill("Done", false, "island-done");
        let (pause, _) = pill(if snap.paused { "Resume" } else { "Pause" }, snap.paused, "island-pause");
        if pause {
            let c = self.client.clone();
            let token = snap.token.clone();
            let paused = !snap.paused;
            crate::net::background(move || c.pause(&token, paused), |_| ());
        }
        if done {
            self.toggle_island(done_ptr, now);
        }
        let _ = Align2::CENTER_CENTER;
    }
}

pub fn lerp_rect(a: Rect, b: Rect, t: f32) -> Rect {
    Rect::from_min_max(a.min + (b.min - a.min) * t, a.max + (b.max - a.max) * t)
}

/// 0 → 1 over `dur` seconds after `delay`.
pub fn fade_progress(elapsed: f64, delay: f64, dur: f64) -> f32 {
    (((elapsed - delay) / dur).clamp(0.0, 1.0)) as f32
}
