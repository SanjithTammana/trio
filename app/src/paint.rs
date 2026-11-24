//! Shared drawing helpers: text runs, eliding, clickable regions, chips, pills.

use crate::theme::Tokens;
use eframe::egui::{
    self, text::LayoutJob, Align2, Color32, CornerRadius, FontId, Galley, Painter, Pos2, Rect, Response, Sense, Stroke, StrokeKind, TextFormat, Ui, Vec2,
    WidgetInfo,
};
use std::sync::Arc;

pub fn fmt(font: FontId, color: Color32) -> TextFormat {
    TextFormat { font_id: font, color, ..Default::default() }
}

/// One line, cut with an ellipsis at `width`.
pub fn line(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(text.to_string(), fmt(font, color));
    elide(&mut job, width);
    ui.fonts_mut(|f| f.layout_job(job))
}

pub fn elide(job: &mut LayoutJob, width: f32) {
    job.wrap.max_width = width.max(1.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
}

/// Wrapped text at `width`.
pub fn wrapped(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<Galley> {
    ui.fonts_mut(|f| f.layout(text.to_string(), font, color, width.max(1.0)))
}

pub fn job(ui: &Ui, job: LayoutJob) -> Arc<Galley> {
    ui.fonts_mut(|f| f.layout_job(job))
}

/// Uppercase with the web UI's 0.06em tracking.
pub fn caps(text: &str, size: f32, color: Color32) -> LayoutJob {
    let mut f = fmt(crate::theme::sans(size), color);
    f.extra_letter_spacing = size * 0.06;
    LayoutJob::single_section(text.to_uppercase(), f)
}

/// A clickable region with an accessible label. Pointer clicks and keyboard activation both
/// count; `pointer` says which one it was (only pointer opens animate).
pub struct Hit {
    pub resp: Response,
}

impl Hit {
    pub fn new(ui: &Ui, rect: Rect, id: egui::Id, label: &str) -> Self {
        let resp = ui.interact(rect, id, Sense::click());
        let label = label.to_string();
        resp.widget_info(|| WidgetInfo::labeled(egui::Role::Button, true, label.clone()));
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        Self { resp }
    }
    pub fn clicked(&self) -> bool {
        self.resp.clicked()
    }
    pub fn pointer(&self) -> bool {
        self.resp.clicked_by(egui::PointerButton::Primary)
    }
    pub fn hot(&self) -> bool {
        self.resp.hovered() || self.resp.has_focus()
    }
}

/// Focus ring, as `:focus-visible` draws it.
pub fn focus_ring(p: &Painter, resp: &Response, t: &Tokens, radius: f32) {
    if resp.has_focus() && resp.ctx.memory(|m| m.focused().is_some()) && !resp.ctx.input(|i| i.pointer.any_down()) {
        p.rect_stroke(resp.rect.expand(2.0), radius + 2.0, Stroke::new(1.5, t.ink), StrokeKind::Outside);
    }
}

/// A mono chip: `text` in a hairline box. Returns its rect.
pub fn chip(ui: &Ui, p: &Painter, at: Pos2, text: &str, t: &Tokens, hot: bool) -> Rect {
    let g = line(ui, text, crate::theme::mono(11.5), if hot { t.ink } else { t.ink2 }, 400.0);
    let rect = Rect::from_min_size(at, g.size() + egui::vec2(10.0, 2.0));
    p.rect_stroke(rect, 4.0, Stroke::new(1.0, if hot { t.ink3 } else { t.line2 }), StrokeKind::Inside);
    p.galley(rect.min + egui::vec2(5.0, 1.0), g, t.ink2);
    rect
}

pub fn chip_size(ui: &Ui, text: &str) -> Vec2 {
    line(ui, text, crate::theme::mono(11.5), Color32::WHITE, 400.0).size() + egui::vec2(10.0, 2.0)
}

/// Text centred in `rect`.
pub fn centered(p: &Painter, rect: Rect, text: &str, font: FontId, color: Color32) {
    p.text(rect.center(), Align2::CENTER_CENTER, text, font, color);
}

pub fn radius(r: f32) -> CornerRadius {
    CornerRadius::same(r.round().clamp(0.0, 255.0) as u8)
}
