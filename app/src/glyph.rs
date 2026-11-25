//! Small glyphs instead of colour: participants by shape (you ■, Claude ●, Codex ▲, Trio ◦),
//! state by outline (open ○, claimed ◐, review ◇, changes ◆ accent, pass ✓, blocked ⊘ accent,
//! branch ⑂, merged ◉, component ▭). Drawn in a 10×10 box, like the web UI's SVG glyphs.

use eframe::egui::{epaint::CubicBezierShape, pos2, Color32, Painter, Pos2, Rect, Shape, Stroke};

const W: f32 = 1.4;

struct Box10 {
    o: Pos2,
    s: f32,
}

impl Box10 {
    fn at(rect: Rect) -> Self {
        let s = rect.width().min(rect.height()) / 10.0;
        Self { o: rect.center() - eframe::egui::vec2(5.0 * s, 5.0 * s), s }
    }
    fn p(&self, x: f32, y: f32) -> Pos2 {
        pos2(self.o.x + x * self.s, self.o.y + y * self.s)
    }
    fn r(&self, r: f32) -> f32 {
        r * self.s
    }
    fn stroke(&self, c: Color32) -> Stroke {
        Stroke::new(W * self.s, c)
    }
    fn poly(&self, pts: &[(f32, f32)]) -> Vec<Pos2> {
        pts.iter().map(|&(x, y)| self.p(x, y)).collect()
    }
    fn shape(&self, p: &Painter, pts: &[(f32, f32)], filled: bool, c: Color32) {
        let v = self.poly(pts);
        if filled {
            p.add(Shape::convex_polygon(v, c, Stroke::NONE));
        } else {
            p.add(Shape::closed_line(v, self.stroke(c)));
        }
    }
    fn circle(&self, p: &Painter, x: f32, y: f32, r: f32, filled: bool, c: Color32) {
        if filled {
            p.circle_filled(self.p(x, y), self.r(r), c);
        } else {
            p.circle_stroke(self.p(x, y), self.r(r), self.stroke(c));
        }
    }
}

/// A participant's glyph; hollow when `to` (addressed, not sender). `me` is the room's human.
pub fn who(p: &Painter, rect: Rect, who: &str, me: &str, to: bool, c: Color32) {
    let b = Box10::at(rect);
    let filled = !to;
    let kind = if who == me { "me".to_string() } else { crate::model::glyph_of(who) };
    match kind.as_str() {
        "me" => b.shape(p, &[(1.0, 1.0), (9.0, 1.0), (9.0, 9.0), (1.0, 9.0)], filled, c),
        "claude" => b.circle(p, 5.0, 5.0, 4.2, filled, c),
        "codex" => b.shape(p, &[(5.0, 0.8), (9.4, 9.0), (0.6, 9.0)], filled, c),
        "trio" => b.circle(p, 5.0, 5.0, 2.4, false, c),
        "hex" => b.shape(p, &[(2.7, 1.0), (7.3, 1.0), (9.6, 5.0), (7.3, 9.0), (2.7, 9.0), (0.4, 5.0)], filled, c),
        "down" => b.shape(p, &[(0.6, 1.0), (9.4, 1.0), (5.0, 9.2)], filled, c),
        "pent" => b.shape(p, &[(5.0, 0.6), (9.4, 3.8), (7.7, 9.2), (2.3, 9.2), (0.6, 3.8)], filled, c),
        "plus" => {
            b.shape(p, &[(3.6, 0.8), (6.4, 0.8), (6.4, 9.2), (3.6, 9.2)], filled, c);
            b.shape(p, &[(0.8, 3.6), (9.2, 3.6), (9.2, 6.4), (0.8, 6.4)], filled, c);
        }
        "bar" => b.shape(p, &[(0.8, 3.0), (9.2, 3.0), (9.2, 7.0), (0.8, 7.0)], filled, c),
        "ring" => {
            b.circle(p, 5.0, 5.0, 4.2, false, c);
            b.circle(p, 5.0, 5.0, 1.8, filled, c);
        }
        "both" | "all" => {
            b.circle(p, 3.2, 5.0, 2.6, filled, c);
            b.shape(p, &[(7.2, 2.2), (9.6, 7.8), (4.8, 7.8)], filled, c);
        }
        _ => b.circle(p, 5.0, 5.0, 3.0, filled, c),
    }
}

/// True for states drawn in the accent colour.
pub fn accented(state: &str, hot: bool) -> bool {
    hot || matches!(state, "changes" | "blocked")
}

/// A work state's glyph. `c` is the ink colour; accent states use `accent`.
pub fn state(p: &Painter, rect: Rect, state: &str, hot: bool, c: Color32, accent: Color32) {
    let b = Box10::at(rect);
    let c = if accented(state, hot) { accent } else { c };
    match state {
        "claimed" => {
            b.circle(p, 5.0, 5.0, 4.0, false, c);
            // Left half filled: the arc from the top through the left to the bottom.
            let half: Vec<(f32, f32)> = (0..=16)
                .map(|i| {
                    let a = std::f32::consts::FRAC_PI_2 + std::f32::consts::PI * i as f32 / 16.0;
                    (5.0 + 4.0 * a.cos(), 5.0 - 4.0 * a.sin())
                })
                .collect();
            b.shape(p, &half, true, c);
        }
        "review" => b.shape(p, &[(5.0, 0.8), (9.2, 5.0), (5.0, 9.2), (0.8, 5.0)], false, c),
        "changes" => b.shape(p, &[(5.0, 0.6), (9.4, 5.0), (5.0, 9.4), (0.6, 5.0)], true, c),
        "pass" => {
            p.add(Shape::line(b.poly(&[(1.2, 5.4), (4.0, 8.2), (9.0, 1.8)]), b.stroke(c)));
        }
        "blocked" => {
            b.circle(p, 5.0, 5.0, 4.0, false, c);
            p.line_segment([b.p(2.2, 7.8), b.p(7.8, 2.2)], b.stroke(c));
        }
        "branch" => {
            for (x, y) in [(2.6, 2.0), (2.6, 8.4), (7.8, 3.6)] {
                b.circle(p, x, y, 1.5, true, c);
            }
            p.line_segment([b.p(2.6, 2.0), b.p(2.6, 8.4)], b.stroke(c));
            p.add(CubicBezierShape::from_points_stroke([b.p(2.6, 7.0), b.p(2.6, 5.0), b.p(7.8, 6.0), b.p(7.8, 3.6)], false, Color32::TRANSPARENT, b.stroke(c)));
        }
        "merged" => {
            b.circle(p, 5.0, 5.0, 4.0, false, c);
            b.circle(p, 5.0, 5.0, 1.6, true, c);
        }
        "component" => {
            let r = Rect::from_min_max(b.p(1.0, 2.0), b.p(9.0, 8.0));
            p.rect_stroke(r, b.r(1.0), b.stroke(c), eframe::egui::StrokeKind::Middle);
        }
        _ => b.circle(p, 5.0, 5.0, 4.0, false, c),
    }
}

/// The window icon: the three participant glyphs on a dark rounded tile (the web favicon),
/// rasterized with 4×4 supersampling.
pub fn icon(size: u32) -> eframe::egui::IconData {
    let n = size as f32;
    let k = n / 32.0;
    let bg = [0x11u8, 0x13, 0x14];
    let fg = [0xf6u8, 0xf7, 0xf7];
    let inside_tile = |x: f32, y: f32| {
        let r = 8.0 * k;
        let cx = x.clamp(r, n - r);
        let cy = y.clamp(r, n - r);
        (x - cx).powi(2) + (y - cy).powi(2) <= r * r
    };
    let tri = |x: f32, y: f32| {
        let (a, b, c) = ((24.0 * k, 12.6 * k), (27.6 * k, 19.2 * k), (20.4 * k, 19.2 * k));
        let s = |p: (f32, f32), q: (f32, f32)| (q.0 - p.0) * (y - p.1) - (q.1 - p.1) * (x - p.0);
        let (d1, d2, d3) = (s(a, b), s(b, c), s(c, a));
        !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
    };
    let glyph = |x: f32, y: f32| {
        let sq = (5.0 * k..11.0 * k).contains(&x) && (13.0 * k..19.0 * k).contains(&y);
        let ci = (x - 16.0 * k).powi(2) + (y - 16.0 * k).powi(2) <= (3.4 * k).powi(2);
        sq || ci || tri(x, y)
    };
    let mut rgba = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut cov, mut ink) = (0.0f32, 0.0f32);
            for sy in 0..4 {
                for sx in 0..4 {
                    let (x, y) = (px as f32 + (sx as f32 + 0.5) / 4.0, py as f32 + (sy as f32 + 0.5) / 4.0);
                    if inside_tile(x, y) {
                        cov += 1.0 / 16.0;
                        if glyph(x, y) {
                            ink += 1.0 / 16.0;
                        }
                    }
                }
            }
            let t = if cov > 0.0 { ink / cov } else { 0.0 };
            for i in 0..3 {
                rgba.push((bg[i] as f32 * (1.0 - t) + fg[i] as f32 * t).round() as u8);
            }
            rgba.push((cov * 255.0).round() as u8);
        }
    }
    eframe::egui::IconData { rgba, width: size, height: size }
}
