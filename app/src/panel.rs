//! The detail panel's content: a message (with its detail, fetched on open), an item or branch
//! (state, links, changes since merge-base, its story), or a component (who changes it).

use crate::app::{Panel, Pick, TrioApp};
use crate::model::{components, name_of, state_label, time_label, Snapshot};
use crate::paint::{self, Hit};
use crate::theme::{self, Tokens};
use crate::glyph;
use eframe::egui::{self, pos2, text::LayoutJob, vec2, Id, Rect, Stroke, StrokeKind, Ui, UiBuilder};
use std::collections::HashSet;

/// Laid-out panel content: a cursor that advances down the column.
struct Col<'a> {
    ui: &'a mut Ui,
    x: f32,
    w: f32,
    y: f32,
}

impl Col<'_> {
    fn gap(&mut self, h: f32) {
        self.y += h;
    }
    fn text(&mut self, job: LayoutJob) -> Rect {
        let mut job = job;
        job.wrap.max_width = self.w;
        let g = paint::job(self.ui, job);
        let r = Rect::from_min_size(pos2(self.x, self.y), g.size());
        self.ui.painter().galley(r.min, g, egui::Color32::WHITE);
        self.y = r.bottom();
        r
    }
    fn section(&mut self, label: &str, t: &Tokens) {
        self.gap(20.0);
        self.text(paint::caps(label, 11.0, t.ink3));
        self.gap(6.0);
    }
}

fn eyebrow(c: &mut Col, t: &Tokens, parts: &[(Option<(&str, bool)>, String)], me: &str) {
    // parts: optional glyph (who-or-state, is_state) then text.
    let mut x = c.x;
    let h = 16.0;
    for (g, text) in parts {
        if let Some((what, is_state)) = g {
            let r = Rect::from_min_size(pos2(x, c.y + 3.0), vec2(10.0, 10.0));
            if *is_state {
                let (st, hot) = what.split_once('!').map_or((*what, false), |(s, _)| (s, true));
                glyph::state(c.ui.painter(), r, st, hot, t.ink3, t.accent);
            } else {
                glyph::who(c.ui.painter(), r, what, me, false, t.ink3);
            }
            x += 16.0;
        }
        let g = paint::line(c.ui, text, theme::sans(12.0), t.ink3, (c.x + c.w - x).max(10.0));
        c.ui.painter().galley(pos2(x, c.y + h / 2.0 - g.size().y / 2.0), g.clone(), t.ink3);
        x += g.size().x + 6.0;
    }
    c.y += h + 6.0;
}

fn heading(c: &mut Col, text: &str, mono: bool, t: &Tokens) {
    let font = if mono { theme::mono(14.5) } else { theme::bold(16.0) };
    let w = c.w - 32.0;
    let g = paint::wrapped(c.ui, text, font, t.ink, w);
    c.ui.painter().galley(pos2(c.x, c.y), g.clone(), t.ink);
    c.y += g.size().y + 12.0;
}

/// A wrapping row of chip buttons. Returns the id of a clicked chip.
fn chips(c: &mut Col, ids: &[String], t: &Tokens, salt: &str) -> Option<String> {
    let mut x = c.x;
    let mut y = c.y;
    let mut hit_id = None;
    let mut row_h: f32 = 0.0;
    for (k, id) in ids.iter().enumerate() {
        let s = paint::chip_size(c.ui, id);
        if x + s.x > c.x + c.w && x > c.x {
            x = c.x;
            y += row_h + 4.0;
        }
        let r = Rect::from_min_size(pos2(x, y), s);
        let hit = Hit::new(c.ui, r, Id::unique(("chip", salt, k)), id);
        paint::chip(c.ui, c.ui.painter(), r.min, id, t, hit.hot());
        paint::focus_ring(c.ui.painter(), &hit.resp, t, 4.0);
        if hit.clicked() {
            hit_id = Some(id.clone());
        }
        x = r.right() + 4.0;
        row_h = row_h.max(s.y);
    }
    c.y = y + row_h;
    hit_id
}

/// Bars: a label, a count, and a 2px fill scaled to the largest. Returns the clicked label.
fn bars(c: &mut Col, rows: &[(String, u32, u32, bool)], t: &Tokens, salt: &str) -> Option<String> {
    let max = rows.iter().map(|r| r.1).max().unwrap_or(1).max(1) as f32;
    let mut out = None;
    for (k, (label, lines, files, hot)) in rows.iter().enumerate() {
        let r = Rect::from_min_size(pos2(c.x, c.y), vec2(c.w, 22.0));
        let hit = Hit::new(c.ui, r, Id::unique(("bar", salt, k)), label);
        let ink = if *hot { t.accent } else { t.ink };
        let n = format!("{lines} · {files}f");
        let ng = paint::line(c.ui, &n, theme::mono(11.5), if *hot { t.accent } else { t.ink3 }, 120.0);
        let mut x = c.x;
        if *hot && salt == "touch" {
            glyph::state(c.ui.painter(), Rect::from_min_size(pos2(x, r.top() + 4.0), vec2(10.0, 10.0)), "component", true, t.ink, t.accent);
            x += 16.0;
        }
        let mut f = paint::fmt(theme::mono(12.5), ink);
        if hit.hot() {
            f.underline = Stroke::new(1.0, t.line2);
        }
        let mut job = LayoutJob::single_section(label.clone(), f);
        paint::elide(&mut job, c.x + c.w - x - ng.size().x - 8.0);
        let lg = paint::job(c.ui, job);
        let p = c.ui.painter();
        p.galley(pos2(x, r.top() + 9.0 - lg.size().y / 2.0), lg, ink);
        p.galley(pos2(r.right() - ng.size().x, r.top() + 9.0 - ng.size().y / 2.0), ng, t.ink3);
        let fill = Rect::from_min_size(pos2(c.x, r.top() + 19.0), vec2(c.w, 2.0));
        p.rect_filled(fill, 0.0, t.line);
        p.rect_filled(Rect::from_min_size(fill.min, vec2(c.w * *lines as f32 / max, 2.0)), 0.0, if *hot { t.accent } else { t.ink2 });
        paint::focus_ring(p, &hit.resp, t, 4.0);
        if hit.clicked() {
            out = Some(label.clone());
        }
        c.y = r.bottom() + 7.0;
    }
    out
}

pub fn show(ui: &mut Ui, app: &mut TrioApp, snap: &Snapshot, panel: &Panel, t: &Tokens, day: &str, rect: Rect) -> Option<Panel> {
    let me = snap.me.as_str();
    let mut nav = None;
    let inner = Rect::from_min_max(rect.min + vec2(20.0, 18.0), rect.max - vec2(20.0, 0.0));
    let mut su = ui.new_child(UiBuilder::new().max_rect(inner).id_salt(("panel-body", panel.key())));
    egui::ScrollArea::vertical().id_salt(("panel-scroll", panel.key())).auto_shrink([false, false]).show(&mut su, |ui| {
        let x = ui.max_rect().left();
        let w = ui.max_rect().width();
        let top = ui.max_rect().top();
        let mut c = Col { ui, x, w, y: top };
        let touches = &snap.graph.touches;
        let merged: HashSet<&str> = snap.graph.merged.iter().map(String::as_str).collect();
        let open_branches: Vec<&str> = snap.graph.items.iter().filter(|n| n.kind == "branch" && !merged.contains(n.id.as_str())).map(|n| n.id.as_str()).collect();

        match panel {
            Panel::Message(m) => {
                let mut parts = vec![(Some((m.from.as_str(), false)), format!("{} · {}", name_of(&m.from, me), time_label(&m.at, day)))];
                if !m.mentions.is_empty() {
                    parts[0].1.push_str(&format!(" · to {}", m.mentions.iter().map(|w| name_of(w, me)).collect::<Vec<_>>().join(", ")));
                }
                eyebrow(&mut c, t, &parts, me);
                heading(&mut c, &m.summary, false, t);
                if !m.items.is_empty() {
                    if let Some(id) = chips(&mut c, &m.items, t, "msg-items") {
                        nav = Some(Panel::Pick(Pick::Item(id)));
                    }
                    c.gap(12.0);
                }
                if m.has_detail {
                    // Keyed by the server's token too: a fetch that completes after a reconnect
                    // writes under the old server's key, which is never read again.
                    let key = format!("{}\n{}", snap.token, m.id);
                    let detail = {
                        let mut d = app.details.lock().expect("details");
                        match d.get(&key) {
                            Some(v) => v.clone(),
                            None => {
                                d.insert(key.clone(), None);
                                let (client, id, slot, ctx) = (app.client().clone(), m.id.clone(), app.details.clone(), c.ui.ctx().clone());
                                crate::net::background(move || client.detail(&id), move |r| {
                                    match r {
                                        Ok(text) => {
                                            slot.lock().expect("details").insert(key, Some(text));
                                        }
                                        // Retryable: forget the attempt after a pause, so the next
                                        // frame with this message open asks again.
                                        Err(_) => {
                                            std::thread::sleep(std::time::Duration::from_secs(2));
                                            slot.lock().expect("details").remove(&key);
                                        }
                                    }
                                    ctx.request_repaint();
                                });
                                None
                            }
                        }
                    };
                    match detail {
                        Some(text) => markdown(&mut c, &text, t),
                        None => {
                            c.text(LayoutJob::single_section("loading…".into(), paint::fmt(theme::sans(14.0), t.ink3)));
                        }
                    }
                } else {
                    c.text(LayoutJob::single_section("No more detail.".into(), paint::fmt(theme::sans(14.0), t.ink3)));
                }
            }
            Panel::Pick(Pick::Component(part)) => {
                let comps = components(touches, &open_branches);
                let Some(comp) = comps.into_iter().find(|x| &x.part == part) else {
                    c.text(LayoutJob::single_section(format!("No open branch changes {part} now."), paint::fmt(theme::sans(14.0), t.ink3)));
                    return;
                };
                let st = if comp.hot { "component!hot" } else { "component" };
                eyebrow(&mut c, t, &[(Some((st, true)), (if comp.hot { "hot spot" } else { "component" }).to_string())], me);
                heading(&mut c, &comp.part, true, t);
                let plural = |n: usize, one: &str, many: &str| if n == 1 { one.to_string() } else { many.to_string() };
                let mut lede = format!(
                    "{} lines in {} file{} across {} open branch{}.",
                    comp.lines,
                    comp.files,
                    plural(comp.files as usize, "", "s"),
                    comp.branches.len(),
                    plural(comp.branches.len(), "", "es")
                );
                if comp.hot {
                    lede.push_str(" More than one open branch changes it, so whichever merges second may conflict.");
                }
                c.text(LayoutJob::single_section(lede, paint::fmt(theme::sans(14.0), t.ink2)));
                c.gap(14.0);
                c.section("Branches", t);
                let mut rows: Vec<_> = comp.branches.iter().map(|b| (b.branch.clone(), b.lines, b.files, comp.hot)).collect();
                rows.sort_by_key(|r| std::cmp::Reverse(r.1));
                if let Some(b) = bars(&mut c, &rows, t, "comp") {
                    nav = Some(Panel::Pick(Pick::Branch(b)));
                }
            }
            Panel::Pick(pick) => {
                let id = match pick {
                    Pick::Item(id) | Pick::Branch(id) => id,
                    Pick::Component(_) => unreachable!(),
                };
                let Some(n) = snap.graph.items.iter().find(|x| &x.id == id) else {
                    c.text(LayoutJob::single_section(format!("{id} is outside the current window."), paint::fmt(theme::sans(14.0), t.ink3)));
                    return;
                };
                let is_branch = n.kind == "branch";
                let linked: Vec<String> = snap
                    .graph
                    .edges
                    .iter()
                    .filter(|e| e.kind == "implements" && if is_branch { e.from == n.id } else { e.to == n.id })
                    .map(|e| if is_branch { e.to.clone() } else { e.from.clone() })
                    .collect();
                let hot_parts: HashSet<String> = components(touches, &open_branches).into_iter().filter(|x| x.hot).map(|x| x.part).collect();
                let state = if is_branch && merged.contains(n.id.as_str()) { "merged" } else { n.state.as_str() };
                let updates = format!("{} update{}", n.touches, if n.touches == 1 { "" } else { "s" });
                eyebrow(&mut c, t, &[(Some((state, true)), format!("{} ·", state_label(state))), (Some((n.lane.as_str(), false)), format!("{} · {updates}", name_of(&n.lane, me)))], me);
                heading(&mut c, &n.id, true, t);
                if !n.summary.is_empty() {
                    c.text(LayoutJob::single_section(n.summary.clone(), paint::fmt(theme::sans(14.0), t.ink2)));
                    c.gap(14.0);
                }
                if !linked.is_empty() {
                    c.section(if is_branch { "Implements" } else { "Branches" }, t);
                    if let Some(x) = chips(&mut c, &linked, t, "linked") {
                        nav = Some(Panel::Pick(if is_branch { Pick::Item(x) } else { Pick::Branch(x) }));
                    }
                }
                let ts = touches.get(&n.id).cloned().unwrap_or_default();
                if is_branch && !ts.is_empty() {
                    c.section("Changes since merge-base", t);
                    let rows: Vec<_> = ts.iter().map(|x| (x.part.clone(), x.lines, x.files, hot_parts.contains(&x.part))).collect();
                    if let Some(part) = bars(&mut c, &rows, t, "touch") {
                        nav = Some(Panel::Pick(Pick::Component(part)));
                    }
                }
                c.section("Story", t);
                for (k, s) in n.steps.iter().rev().enumerate() {
                    let what = paint::wrapped(c.ui, &s.what, theme::sans(14.0), t.ink, c.w - 22.0);
                    let meta = paint::line(c.ui, &format!("{} · {}", name_of(&s.who, me), time_label(&s.at, day)), theme::sans(11.5), t.ink3, c.w - 22.0);
                    let h = 6.0 + what.size().y + 1.0 + meta.size().y + 6.0;
                    let row = Rect::from_min_size(pos2(c.x, c.y), vec2(c.w, h));
                    let step_rect = Rect::from_min_max(pos2(c.x + 22.0, row.top()), row.max);
                    let hit = Hit::new(c.ui, step_rect, Id::unique(("step", &s.event, k)), &s.what);
                    let p = c.ui.painter();
                    p.line_segment([row.left_top(), row.right_top()], Stroke::new(1.0, t.line));
                    glyph::who(p, Rect::from_min_size(pos2(c.x + 2.0, row.top() + 11.0), vec2(10.0, 10.0)), &s.who, me, false, t.ink2);
                    let what = if hit.hot() {
                        let mut f = paint::fmt(theme::sans(14.0), t.ink);
                        f.underline = Stroke::new(1.0, t.line2);
                        let mut job = LayoutJob::single_section(s.what.clone(), f);
                        job.wrap.max_width = c.w - 22.0;
                        paint::job(c.ui, job)
                    } else {
                        what
                    };
                    p.galley(pos2(step_rect.left(), row.top() + 6.0), what.clone(), t.ink);
                    p.galley(pos2(step_rect.left(), row.top() + 7.0 + what.size().y), meta, t.ink3);
                    paint::focus_ring(p, &hit.resp, t, 4.0);
                    if hit.clicked()
                        && let Some(msg) = snap.messages.iter().find(|x| x.id == s.event)
                    {
                        nav = Some(Panel::Message(msg.clone()));
                    }
                    c.y = row.bottom();
                }
            }
        }
        c.gap(28.0);
        let used = c.y - top;
        c.ui.allocate_exact_size(vec2(w, used.max(0.0)), egui::Sense::hover());
    });
    let _ = StrokeKind::Inside;
    nav
}

/// A tiny markdown renderer for detail text: headings, bullets, fences, `code`, **bold**.
/// Builds laid-out text only; untrusted text never becomes markup.
fn markdown(c: &mut Col, text: &str, t: &Tokens) {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();
        let lead = line.len() - trimmed.len();
        if lead <= 3 && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            let fence: String = trimmed.chars().take_while(|&ch| ch == trimmed.chars().next().unwrap_or('`')).collect();
            let mut body = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].trim().starts_with(&fence) {
                body.push(lines[i]);
                i += 1;
            }
            i += 1;
            c.gap(6.0);
            let g = paint::wrapped(c.ui, &body.join("\n"), theme::mono(12.5), t.ink, c.w - 24.0);
            let r = Rect::from_min_size(pos2(c.x, c.y), vec2(c.w, g.size().y + 20.0));
            c.ui.painter().rect_filled(r, 6.0, t.sunk);
            c.ui.painter().galley(pos2(c.x + 12.0, c.y + 10.0), g, t.ink);
            c.y = r.bottom() + 6.0;
            continue;
        }
        if is_bullet(line) {
            c.gap(6.0);
            while i < lines.len() && is_bullet(lines[i]) {
                let item = lines[i].trim_start()[2..].to_string();
                let bullet = Rect::from_min_size(pos2(c.x + 6.0, c.y + 9.0), vec2(4.0, 4.0));
                c.ui.painter().circle_filled(bullet.center(), 2.0, t.ink);
                let (x, w) = (c.x, c.w);
                c.x += 18.0;
                c.w -= 18.0;
                c.text(inline(&item, 14.0, t, false));
                c.x = x;
                c.w = w;
                i += 1;
            }
            c.gap(6.0);
            continue;
        }
        let hashes = trimmed.chars().take_while(|&ch| ch == '#').count();
        if (1..=4).contains(&hashes) && lead == 0 && trimmed[hashes..].starts_with(char::is_whitespace) {
            c.gap(14.0);
            c.text(inline(trimmed[hashes..].trim(), 13.5, t, true));
            c.gap(4.0);
        } else if !line.trim().is_empty() {
            c.gap(6.0);
            c.text(inline(line, 14.0, t, false));
            c.gap(6.0);
        }
        i += 1;
    }
}

fn is_bullet(line: &str) -> bool {
    let s = line.trim_start();
    s.starts_with("- ") || s.starts_with("* ")
}

/// `code` and **bold** spans inside one line.
fn inline(text: &str, size: f32, t: &Tokens, heading: bool) -> LayoutJob {
    let base = if heading { theme::bold(size) } else { theme::sans(size) };
    let mut job = LayoutJob::default();
    let mut rest = text;
    while !rest.is_empty() {
        let tick = rest.find('`');
        let star = rest.find("**");
        let next = match (tick, star) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let Some(at) = next else {
            job.append(rest, 0.0, paint::fmt(base.clone(), t.ink));
            break;
        };
        let (marker, close) = if rest[at..].starts_with('`') { ("`", "`") } else { ("**", "**") };
        match rest[at + marker.len()..].find(close).filter(|&n| n > 0) {
            Some(n) => {
                job.append(&rest[..at], 0.0, paint::fmt(base.clone(), t.ink));
                let inner = &rest[at + marker.len()..at + marker.len() + n];
                if marker == "`" {
                    let mut f = paint::fmt(theme::mono(12.5), t.ink);
                    f.background = t.sunk;
                    f.expand_bg = 1.0;
                    job.append(inner, 0.0, f);
                } else {
                    job.append(inner, 0.0, paint::fmt(theme::bold(size), t.ink));
                }
                rest = &rest[at + marker.len() + n + close.len()..];
            }
            None => {
                job.append(&rest[..at + marker.len()], 0.0, paint::fmt(base.clone(), t.ink));
                rest = &rest[at + marker.len()..];
            }
        }
    }
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_spans_split_code_and_bold() {
        let job = inline("run `cargo test` then **ship** it", 14.0, &crate::theme::LIGHT, false);
        let parts: Vec<&str> = job.sections.iter().map(|s| &job.text[s.byte_range.start.0..s.byte_range.end.0]).collect();
        assert_eq!(parts, ["run ", "cargo test", " then ", "ship", " it"]);
        let open = inline("a ` b", 14.0, &crate::theme::LIGHT, false);
        assert_eq!(open.text, "a ` b");
    }
}
