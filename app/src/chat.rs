//! The group chat as a lane log: a three-column gutter (you, Claude, Codex) carries each line's
//! sender as a solid glyph in its own lane and anyone it addresses as a hollow one, so who is
//! talking to whom reads down the page without colour. Click a line for its detail.

use crate::app::{Open, Panel, TrioApp};
use crate::model::{lanes, name_of, time_label, Message, Snapshot};
use crate::paint::{self, Hit};
use crate::theme::{self, mix, Tokens};
use crate::glyph;
use eframe::egui::{self, pos2, vec2, Id, Key, KeyboardShortcut, Modifiers, Painter, Rect, Stroke, StrokeKind, UiBuilder};
use std::sync::{Arc, Mutex};

const MAX: usize = 140;
const COL: f32 = 880.0;
const GUT: f32 = 60.0;
const GAP: f32 = 12.0;
const HEAD: f32 = 32.0;

#[derive(Default)]
pub struct ChatState {
    pub draft: String,
    pub focus: bool,
    busy: bool,
    error: Option<String>,
    sent: Arc<Mutex<Option<Result<(), String>>>>,
    to_bottom: bool,
    composer_h: f32,
}

/// Lane hairlines and the sender/addressee glyphs for one row.
#[allow(clippy::too_many_arguments)]
fn gutter(p: &Painter, rect: Rect, from: &str, to: &[String], me: &str, t: &Tokens, top: f32, sys: bool) {
    let lane_c = mix(t.line, 0.4, t.line2);
    for i in 0..3 {
        let x = rect.left() + 9.5 + 20.0 * i as f32;
        p.line_segment([pos2(x, rect.top()), pos2(x, rect.bottom())], Stroke::new(1.0, lane_c));
    }
    for (i, l) in lanes(me).iter().enumerate() {
        let sender = from == l;
        let addressed = !sender && (to.iter().any(|w| w == l) || (to.iter().any(|w| w == "both") && l != me));
        if !(sender || addressed) {
            continue;
        }
        let g = Rect::from_min_size(pos2(rect.left() + 20.0 * i as f32 + 5.0, rect.top() + top), vec2(10.0, 10.0));
        p.rect_filled(g.expand(3.0), 0.0, t.bg);
        let c = if sys || addressed { t.ink3 } else { t.ink };
        glyph::who(p, g, l, me, addressed, c);
    }
}

pub fn show(ui: &mut egui::Ui, app: &mut TrioApp, snap: &Snapshot, t: &Tokens, day: &str) -> Option<Open> {
    let me = snap.me.as_str();
    let area = ui.max_rect();
    let comp_h = if app.chat.composer_h > 0.0 { app.chat.composer_h } else { 110.0 };
    let log_rect = Rect::from_min_max(area.min, pos2(area.right(), area.bottom() - comp_h));
    let comp_rect = Rect::from_min_max(pos2(area.left(), log_rect.bottom()), area.max);
    let rows: Vec<&Message> = snap.messages.iter().filter(|m| !(m.kind == "status" && m.from != "trio")).collect();
    let mut open = None;

    // Settle a finished send.
    if let Some(r) = app.chat.sent.lock().expect("sent").take() {
        app.chat.busy = false;
        match r {
            Ok(()) => {
                app.chat.draft.clear();
                app.chat.error = None;
                app.chat.to_bottom = true;
            }
            Err(e) => app.chat.error = Some(e),
        }
    }

    // ---------- log ----------
    let mut log = ui.new_child(UiBuilder::new().max_rect(log_rect).id_salt("log"));
    log.set_clip_rect(log_rect);
    let to_bottom = std::mem::take(&mut app.chat.to_bottom);
    egui::ScrollArea::vertical().id_salt("chat-log").stick_to_bottom(true).auto_shrink([false, false]).show(&mut log, |ui| {
        let full = ui.max_rect();
        let col_w = COL.min(full.width());
        let left = full.center().x - col_w / 2.0 + 16.0;
        let width = col_w - 32.0;
        let text_x = left + GUT + GAP;
        let text_w = width - GUT - GAP;
        ui.allocate_exact_size(vec2(full.width(), HEAD), egui::Sense::hover());
        if rows.is_empty() {
            let (r, _) = ui.allocate_exact_size(vec2(full.width(), 120.0), egui::Sense::hover());
            let g = paint::wrapped(ui, "Nothing yet. Say something, or type trio in a Claude Code or Codex session in this folder.", theme::sans(14.0), t.ink3, width);
            ui.painter().galley(pos2(r.center().x - g.size().x / 2.0, r.top() + 48.0), g, t.ink3);
        }
        for (i, m) in rows.iter().enumerate() {
            let to = &m.mentions;
            if m.kind != "message" || m.from == "trio" {
                let mut job = egui::text::LayoutJob::default();
                if !m.summary.to_lowercase().starts_with(&m.from) {
                    job.append(&format!("{} ", name_of(&m.from, me)), 0.0, paint::fmt(theme::mono(11.5), t.ink2));
                }
                job.append(&format!("{} · {}", m.summary, time_label(&m.at, day)), 0.0, paint::fmt(theme::mono(11.5), t.ink3));
                job.wrap.max_width = text_w;
                let g = paint::job(ui, job);
                let h = g.size().y + 6.0;
                let (r, _) = ui.allocate_exact_size(vec2(full.width(), h), egui::Sense::hover());
                if ui.is_rect_visible(r) {
                    let p = ui.painter();
                    gutter(p, Rect::from_min_size(pos2(left, r.top()), vec2(GUT, h)), &m.from, to, me, t, 7.0, true);
                    p.galley(pos2(text_x, r.top() + 3.0), g, t.ink3);
                }
                continue;
            }
            let prev = i.checked_sub(1).map(|j| rows[j]);
            let head = prev.is_none_or(|p| p.from != m.from || p.kind != "message" || p.at.get(..15) != m.at.get(..15));
            let mine = m.from == me;
            // Body: summary, then up to four item chips and "more ›" on the last line if they fit.
            let body_w = text_w;
            let g = paint::wrapped(ui, &m.summary, theme::sans(14.0), if mine { t.ink2 } else { t.ink }, body_w);
            let extras: Vec<(String, egui::Vec2)> = m.items.iter().take(4).map(|it| (it.clone(), paint::chip_size(ui, it))).collect();
            let more = m.has_detail.then(|| paint::line(ui, "more ›", theme::sans(12.0), t.ink3, 100.0));
            let extra_w: f32 = extras.iter().map(|(_, s)| s.x + 4.0).sum::<f32>() + if extras.is_empty() { 0.0 } else { 2.0 } + more.as_ref().map_or(0.0, |m| m.size().x + 6.0);
            let last_end = g.rows.last().map_or(0.0, |r| r.pos.x + r.row.size.x);
            let inline = last_end + extra_w <= body_w;
            let line_h = 14.0 * 1.45;
            let body_h = g.size().y.max(line_h) + if inline || extra_w == 0.0 { 0.0 } else { line_h };
            let hdr_h = if head { 3.0 + 18.0 } else { 0.0 };
            let pad = if head { 12.0 } else { 0.0 };
            let h = pad + hdr_h + body_h + 4.0;
            let (r, _) = ui.allocate_exact_size(vec2(full.width(), h), egui::Sense::hover());
            let body_rect = Rect::from_min_size(pos2(text_x - 8.0, r.top() + pad + hdr_h), vec2(body_w + 16.0, body_h + 4.0));
            app.anchors.insert(m.id.clone(), body_rect);
            if !ui.is_rect_visible(r) {
                continue;
            }
            let label = format!("{}: {}. Open detail", name_of(&m.from, me), m.summary);
            let hit = Hit::new(ui, body_rect, Id::unique(("msg", &m.id, i)), &label);
            let p = ui.painter();
            gutter(p, Rect::from_min_size(pos2(left, r.top()), vec2(GUT, h)), &m.from, to, me, t, if head { 21.0 } else { 8.0 }, false);
            if head {
                let ng = paint::line(ui, &name_of(&m.from, me), theme::bold(12.5), t.ink, 300.0);
                let tg = paint::line(ui, &time_label(&m.at, day), theme::mono(11.0), t.ink3, 120.0);
                let y = r.top() + pad + 3.0;
                p.galley(pos2(text_x, y), ng.clone(), t.ink);
                p.galley(pos2(text_x + ng.size().x + 8.0, y + 2.0), tg, t.ink3);
            }
            if hit.hot() {
                p.rect_filled(body_rect, 6.0, t.sunk);
            }
            let bx = text_x;
            let by = body_rect.top() + 2.0;
            p.galley(pos2(bx, by), g.clone(), t.ink);
            let (mut cx, cy) = if inline { (bx + last_end + 6.0, by + g.size().y - line_h) } else { (bx, by + g.size().y) };
            for (it, s) in &extras {
                paint::chip(ui, p, pos2(cx, cy + (line_h - s.y) / 2.0), it, t, false);
                cx += s.x + 4.0;
            }
            if let Some(mg) = more {
                cx += if extras.is_empty() { 0.0 } else { 2.0 };
                p.galley(pos2(cx, cy + (line_h - mg.size().y) / 2.0), mg, t.ink3);
            }
            paint::focus_ring(p, &hit.resp, t, 6.0);
            if hit.clicked() {
                open = Some(Open { panel: Panel::Message((*m).clone()), from: body_rect, pointer: hit.pointer() });
            }
        }
        ui.allocate_exact_size(vec2(full.width(), 20.0), egui::Sense::hover());
        if to_bottom {
            ui.scroll_to_cursor(Some(egui::Align::BOTTOM));
        }

        // Sticky lanes head, painted over whatever scrolled under it.
        let clip = ui.clip_rect();
        let head = Rect::from_min_size(pos2(full.left(), clip.top()), vec2(full.width(), HEAD));
        let p = ui.painter().with_clip_rect(clip);
        p.rect_filled(head, 0.0, t.bg);
        for (i, l) in lanes(me).iter().enumerate() {
            let g = Rect::from_min_size(pos2(left + 20.0 * i as f32 + 5.0, head.top() + 13.0), vec2(10.0, 10.0));
            glyph::who(&p, g, l, me, false, t.ink3);
            let tip = ui.interact(g.expand(5.0), Id::unique(("lane", i)), egui::Sense::hover());
            tip.on_hover_text(name_of(l, me));
        }
        if !rows.is_empty() {
            let lg = paint::line(ui, &format!("{} · Claude · Codex", name_of(me, me)), theme::sans(11.0), t.ink3, text_w);
            p.galley(pos2(text_x, head.top() + 18.0 - lg.size().y / 2.0), lg, t.ink3);
        }
    });

    // ---------- composer ----------
    let p = ui.painter();
    p.rect_filled(comp_rect, 0.0, t.bg);
    p.line_segment([comp_rect.left_top(), comp_rect.right_top()], Stroke::new(1.0, t.line));
    let col_w = COL.min(comp_rect.width());
    let inner = Rect::from_min_max(pos2(comp_rect.center().x - col_w / 2.0 + 16.0, comp_rect.top() + 10.0), pos2(comp_rect.center().x + col_w / 2.0 - 16.0, comp_rect.bottom() - 14.0));
    let mut cu = ui.new_child(UiBuilder::new().max_rect(inner).id_salt("composer"));
    let start_y = inner.top();

    // Tools: @mentions and the length counter.
    let mut x = inner.left();
    let tools_y = start_y;
    for w in ["claude", "codex", "both"] {
        let label = format!("@{w}");
        let g = paint::line(&cu, &label, theme::mono(12.0), t.ink2, 100.0);
        let r = Rect::from_min_size(pos2(x, tools_y), g.size() + vec2(14.0, 2.0));
        x = r.right() + 4.0;
        let hit = Hit::new(&cu, r, Id::unique(("mention", w)), &format!("Mention {}", name_of(w, me)));
        let p = cu.painter();
        p.rect_stroke(r, 5.0, Stroke::new(1.0, if hit.hot() { t.line2 } else { t.line }), StrokeKind::Inside);
        p.galley(pos2(r.left() + 7.0, r.top() + 1.0), paint::line(&cu, &label, theme::mono(12.0), if hit.hot() { t.ink } else { t.ink2 }, 100.0), t.ink2);
        paint::focus_ring(p, &hit.resp, t, 5.0);
        if hit.clicked() {
            let d = &mut app.chat.draft;
            if !d.contains(&label) {
                *d = format!("{label} {d}").trim_end().to_string() + " ";
            }
            app.chat.focus = true;
        }
    }
    let n = app.chat.draft.chars().count();
    let over = n > MAX;
    let count = if over { "long: the first sentence becomes the line, the rest its detail".to_string() } else { format!("{n}/{MAX}") };
    let cg = paint::line(&cu, &count, theme::mono(11.0), if over { t.accent } else { t.ink3 }, inner.width() - (x - inner.left()) - 8.0);
    cu.painter().galley(pos2(inner.right() - cg.size().x, tools_y + 2.0), cg, t.ink3);

    // The box: textarea and Send.
    let box_top = tools_y + 20.0 + 6.0;
    let send_g = paint::line(&cu, "Send", theme::sans(13.0), t.bg, 100.0);
    let send_w = send_g.size().x + 24.0;
    let text_w = inner.width() - 12.0 - 7.0 - 8.0 - send_w;
    let edit_id = Id::unique("composer-text");
    // Reserve the box shapes now so they sit under the text; filled in once its height is known.
    let box_bg = cu.painter().add(egui::Shape::Noop);
    let box_line = cu.painter().add(egui::Shape::Noop);
    let mut eu = cu.new_child(UiBuilder::new().max_rect(Rect::from_min_size(pos2(inner.left() + 12.0, box_top + 7.0), vec2(text_w, 180.0))).id_salt("edit"));
    let had_focus = eu.memory(|m| m.has_focus(edit_id));
    let enter = had_focus && eu.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift && !i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))));
    let edit = egui::ScrollArea::vertical().id_salt("composer-scroll").max_height(180.0).auto_shrink([false, true]).show(&mut eu, |ui| {
        ui.add(
            egui::TextEdit::multiline(&mut app.chat.draft)
                .id(edit_id)
                .frame(egui::Frame::NONE)
                .font(theme::sans(14.0))
                .text_color(t.ink)
                .desired_width(text_w)
                .desired_rows(1)
                .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                .hint_text(egui::RichText::new("Message the room   ( / to focus, Enter to send, Shift+Enter for a new line )").color(t.ink3)),
        )
    });
    let resp = edit.inner;
    if app.chat.focus {
        resp.request_focus();
        app.chat.focus = false;
    }
    if resp.has_focus() && eu.input(|i| i.key_pressed(Key::Escape)) {
        resp.surrender_focus();
    }
    let text_h = edit.content_size.y.clamp(24.0, 180.0);
    let box_rect = Rect::from_min_size(pos2(inner.left(), box_top), vec2(inner.width(), text_h + 14.0));
    let p = cu.painter();
    p.set(box_bg, egui::epaint::RectShape::filled(box_rect, 10.0, t.surface));
    p.set(box_line, egui::epaint::RectShape::stroke(box_rect, 10.0, Stroke::new(1.0, if resp.has_focus() { t.ink3 } else { t.line2 }), StrokeKind::Inside));
    let send_rect = Rect::from_min_size(pos2(box_rect.right() - 7.0 - send_w, box_rect.bottom() - 7.0 - 26.0), vec2(send_w, 26.0));
    let can = !app.chat.draft.trim().is_empty() && !app.chat.busy;
    let send_hit = Hit::new(&cu, send_rect, Id::unique("send"), "Send");
    let alpha = if can { 1.0 } else { 0.3 };
    p.rect_filled(send_rect, 7.0, theme::fade(t.ink, alpha));
    p.galley(pos2(send_rect.left() + 12.0, send_rect.center().y - send_g.size().y / 2.0), paint::line(&cu, "Send", theme::sans(13.0), theme::fade(t.bg, alpha.max(0.6)), 100.0), t.bg);
    paint::focus_ring(p, &send_hit.resp, t, 7.0);
    let mut bottom = box_rect.bottom();
    if let Some(e) = &app.chat.error {
        let eg = paint::wrapped(&cu, e, theme::sans(12.5), t.accent, inner.width());
        p.galley(pos2(inner.left(), bottom + 6.0), eg.clone(), t.accent);
        bottom += 6.0 + eg.size().y;
    }
    if (enter || send_hit.clicked()) && can {
        if enter {
            // The Enter keystroke itself must not leave a newline behind.
            while app.chat.draft.ends_with('\n') {
                app.chat.draft.pop();
            }
        }
        app.chat.busy = true;
        app.chat.error = None;
        let c = app.client().clone();
        let token = snap.token.clone();
        let text = app.chat.draft.trim().to_string();
        let slot = app.chat.sent.clone();
        let ctx = ui.ctx().clone();
        crate::net::background(move || c.say(&token, &text), move |r| {
            *slot.lock().expect("sent") = Some(r);
            ctx.request_repaint();
        });
    }
    let h = bottom + 14.0 - comp_rect.top();
    if (h - app.chat.composer_h).abs() > 0.5 {
        app.chat.composer_h = h;
        ui.ctx().request_repaint();
    }
    open
}
