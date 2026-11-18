//! Annotation model: what the user drew, in image pixel coordinates.

use crate::config::{Style, Tool};
use crate::geom::*;
use std::cell::Cell;

#[derive(Clone, Debug)]
pub enum Kind {
    Pen(Vec<P>),
    Marker(Vec<P>),
    Line(P, P),
    Arrow(P, P),
    Rect(P, P),
    Ellipse(P, P),
    /// `ext` = measured layout size, filled in by the painter. `angle` turns the
    /// box about its centre, in degrees clockwise. `wrap` > 0 is a fixed width
    /// the lines wrap in; 0 = as wide as the longest line.
    Text {
        at: P,
        text: String,
        ext: Cell<(f32, f32)>,
        angle: f32,
        wrap: f32,
    },
    Counter {
        at: P,
        n: u32,
    },
    Blur(P, P),
    Pixelate(P, P),
}

pub use crate::config::Align;

/// Turns `v` by `deg` degrees clockwise (y points down).
pub fn rot(v: P, deg: f32) -> P {
    let (s, c) = deg.to_radians().sin_cos();
    p(v.x * c - v.y * s, v.x * s + v.y * c)
}

/// Angle of `v` in degrees, clockwise from +x.
pub fn angle_of(v: P) -> f32 {
    v.y.atan2(v.x).to_degrees()
}

/// Wraps an angle into (-180, 180].
pub fn wrap_deg(a: f32) -> f32 {
    let a = a.rem_euclid(360.0);
    if a > 180.0 {
        a - 360.0
    } else {
        a
    }
}

#[derive(Clone, Debug)]
pub struct Shape {
    pub kind: Kind,
    pub style: Style,
}

impl Shape {
    pub fn tool(&self) -> Tool {
        match self.kind {
            Kind::Pen(_) => Tool::Pen,
            Kind::Marker(_) => Tool::Marker,
            Kind::Line(..) => Tool::Line,
            Kind::Arrow(..) => Tool::Arrow,
            Kind::Rect(..) => Tool::Rect,
            Kind::Ellipse(..) => Tool::Ellipse,
            Kind::Text { .. } => Tool::Text,
            Kind::Counter { .. } => Tool::Counter,
            Kind::Blur(..) => Tool::Blur,
            Kind::Pixelate(..) => Tool::Pixelate,
        }
    }

    pub fn counter_radius(&self) -> f32 {
        self.style.width
    }

    /// Padding of a text box around the glyphs.
    pub fn text_pad(&self) -> f32 {
        (self.style.width * 0.32).max(4.0)
    }

    /// Unturned text box (glyphs plus padding) and its angle. The painter draws
    /// exactly this box, turned about its centre.
    pub fn text_box(&self) -> Option<(RectF, f32)> {
        let Kind::Text { at, ext, angle, .. } = &self.kind else {
            return None;
        };
        let w = self.style.width;
        let (tw, th) = ext.get();
        let pad = self.text_pad();
        let b = RectF::xywh(
            at.x - pad,
            at.y - pad,
            tw.max(w * 0.3) + pad * 2.0,
            th.max(w * 1.25) + pad * 2.0,
        );
        Some((b, *angle))
    }

    /// Corners of the turned text box: top-left, top-right, bottom-right, bottom-left.
    pub fn text_corners(&self) -> Option<[P; 4]> {
        let (b, a) = self.text_box()?;
        let c = p(b.cx(), b.cy());
        Some([p(b.l, b.t), p(b.r, b.t), p(b.r, b.b), p(b.l, b.b)].map(|q| c.add(rot(q.sub(c), a))))
    }

    /// Image point in the text's own (unturned) frame.
    pub fn text_local(&self, q: P) -> P {
        match self.text_box() {
            Some((b, a)) => {
                let c = p(b.cx(), b.cy());
                c.add(rot(q.sub(c), -a))
            }
            None => q,
        }
    }

    /// Rotation handle of a text: the middle of the bottom edge and the knob
    /// `off` pixels below it.
    pub fn rot_handle(&self, off: f32) -> Option<(P, P)> {
        let (b, a) = self.text_box()?;
        let c = p(b.cx(), b.cy());
        let base = c.add(rot(p(0.0, b.b - c.y), a));
        let knob = c.add(rot(p(0.0, b.b - c.y + off), a));
        Some((base, knob))
    }

    /// Middles of the left and right edges of the turned text box.
    pub fn text_sides(&self) -> Option<[P; 2]> {
        let c = self.text_corners()?;
        Some([c[0].lerp(c[3], 0.5), c[1].lerp(c[2], 0.5)])
    }

    /// Scales the font of a text from `orig` by dragging `corner` to `q`; the
    /// opposite corner stays where it is.
    pub fn scale_text(&mut self, orig: &Shape, corner: usize, q: P, range: (f32, f32)) {
        let Some(c0) = orig.text_corners() else {
            return;
        };
        let anchor = c0[(corner + 2) % 4];
        let diag = c0[corner].sub(anchor);
        let d0 = diag.len().max(1.0);
        let u = diag.norm();
        let along = (q.x - anchor.x) * u.x + (q.y - anchor.y) * u.y;
        let k = (along / d0).max(0.02);
        let w0 = orig.style.width;
        let w = (w0 * k).clamp(range.0, range.1).round();
        let k = w / w0;
        *self = orig.clone();
        self.style.width = w;
        let Kind::Text { ext, .. } = &self.kind else {
            return;
        };
        let (tw, th) = ext.get();
        ext.set((tw * k, th * k));
        // Where the anchor corner is now, then move the box back onto it.
        let Some(c1) = self.text_corners() else { return };
        let d = anchor.sub(c1[(corner + 2) % 4]);
        self.translate(d);
    }

    pub fn bounds(&self) -> RectF {
        let w = self.style.width;
        match &self.kind {
            Kind::Pen(pts) | Kind::Marker(pts) => {
                let mut r = RectF::from_pts(pts[0], pts[0]);
                for q in pts {
                    r = r.union(&RectF::from_pts(*q, *q));
                }
                r.inflate(w * 0.5 + 1.0)
            }
            Kind::Line(a, b) => RectF::from_pts(*a, *b).inflate(w * 0.5 + 1.0),
            Kind::Arrow(a, b) => RectF::from_pts(*a, *b).inflate(w * 2.6 + 6.0),
            Kind::Rect(a, b) | Kind::Ellipse(a, b) => {
                RectF::from_pts(*a, *b).inflate(w * 0.5 + 1.0)
            }
            Kind::Blur(a, b) | Kind::Pixelate(a, b) => RectF::from_pts(*a, *b),
            Kind::Text { .. } => {
                let c = self.text_corners().unwrap_or_default();
                let mut r = RectF::from_pts(c[0], c[0]);
                for q in &c[1..] {
                    r = r.union(&RectF::from_pts(*q, *q));
                }
                r
            }
            Kind::Counter { at, .. } => {
                let r = self.counter_radius();
                RectF::new(at.x - r, at.y - r, at.x + r, at.y + r)
            }
        }
    }

    pub fn hit(&self, q: P, tol: f32) -> bool {
        let w = self.style.width;
        let near = w * 0.5 + tol;
        match &self.kind {
            Kind::Pen(pts) | Kind::Marker(pts) => {
                if pts.len() == 1 {
                    return q.dist(pts[0]) <= near;
                }
                pts.windows(2).any(|s| seg_dist(q, s[0], s[1]) <= near)
            }
            Kind::Line(a, b) => seg_dist(q, *a, *b) <= near,
            Kind::Arrow(a, b) => seg_dist(q, *a, *b) <= near.max(w * 1.6 + tol),
            Kind::Rect(a, b) => {
                let r = RectF::from_pts(*a, *b);
                if self.style.fill != crate::config::Fill::None {
                    return r.inflate(near).contains(q);
                }
                r.inflate(near).contains(q) && !r.inflate(-near).contains(q)
            }
            Kind::Ellipse(a, b) => {
                let r = RectF::from_pts(*a, *b);
                let (rx, ry) = (r.w() * 0.5, r.h() * 0.5);
                if rx < 1.0 || ry < 1.0 {
                    return r.inflate(near).contains(q);
                }
                let (dx, dy) = (q.x - r.cx(), q.y - r.cy());
                let d = ((dx / rx).powi(2) + (dy / ry).powi(2)).sqrt();
                let edge = (d - 1.0).abs() * rx.min(ry);
                if self.style.fill != crate::config::Fill::None {
                    d <= 1.0 || edge <= near
                } else {
                    edge <= near
                }
            }
            Kind::Text { .. } => self
                .text_box()
                .is_some_and(|(b, _)| b.inflate(tol).contains(self.text_local(q))),
            Kind::Blur(..) | Kind::Pixelate(..) | Kind::Counter { .. } => {
                self.bounds().inflate(tol).contains(q)
            }
        }
    }

    pub fn translate(&mut self, d: P) {
        match &mut self.kind {
            Kind::Pen(pts) | Kind::Marker(pts) => pts.iter_mut().for_each(|q| *q = q.add(d)),
            Kind::Line(a, b)
            | Kind::Arrow(a, b)
            | Kind::Rect(a, b)
            | Kind::Ellipse(a, b)
            | Kind::Blur(a, b)
            | Kind::Pixelate(a, b) => {
                *a = a.add(d);
                *b = b.add(d);
            }
            Kind::Text { at, .. } | Kind::Counter { at, .. } => *at = at.add(d),
        }
    }

    /// Edit handles: endpoints for lines, corners for boxes.
    pub fn handles(&self) -> Vec<P> {
        match &self.kind {
            Kind::Line(a, b) | Kind::Arrow(a, b) => vec![*a, *b],
            Kind::Rect(a, b) | Kind::Ellipse(a, b) | Kind::Blur(a, b) | Kind::Pixelate(a, b) => {
                let r = RectF::from_pts(*a, *b);
                vec![p(r.l, r.t), p(r.r, r.t), p(r.r, r.b), p(r.l, r.b)]
            }
            _ => Vec::new(),
        }
    }

    pub fn drag_handle(&mut self, i: usize, q: P) {
        match &mut self.kind {
            Kind::Line(a, b) | Kind::Arrow(a, b) => {
                if i == 0 {
                    *a = q
                } else {
                    *b = q
                }
            }
            Kind::Rect(a, b) | Kind::Ellipse(a, b) | Kind::Blur(a, b) | Kind::Pixelate(a, b) => {
                let r = RectF::from_pts(*a, *b);
                let (mut l, mut t, mut rr, mut bb) = (r.l, r.t, r.r, r.b);
                match i {
                    0 => {
                        l = q.x;
                        t = q.y
                    }
                    1 => {
                        rr = q.x;
                        t = q.y
                    }
                    2 => {
                        rr = q.x;
                        bb = q.y
                    }
                    _ => {
                        l = q.x;
                        bb = q.y
                    }
                }
                *a = p(l, t);
                *b = p(rr, bb);
            }
            _ => {}
        }
    }

    /// Too small to keep (an accidental click).
    pub fn is_degenerate(&self) -> bool {
        match &self.kind {
            Kind::Pen(pts) | Kind::Marker(pts) => pts.is_empty(),
            Kind::Line(a, b) | Kind::Arrow(a, b) => a.dist(*b) < 3.0,
            Kind::Rect(a, b) | Kind::Ellipse(a, b) | Kind::Blur(a, b) | Kind::Pixelate(a, b) => {
                (a.x - b.x).abs() < 3.0 || (a.y - b.y).abs() < 3.0
            }
            Kind::Text { text, .. } => text.trim().is_empty(),
            Kind::Counter { .. } => false,
        }
    }
}

/// Snaps `b` so that a-b is a multiple of 45 degrees.
pub fn snap45(a: P, b: P) -> P {
    let d = b.sub(a);
    let ang = d.y.atan2(d.x);
    let step = std::f32::consts::FRAC_PI_4;
    let s = (ang / step).round() * step;
    let l = d.len();
    a.add(p(s.cos() * l, s.sin() * l))
}

/// Makes the box a-b square.
pub fn square(a: P, b: P) -> P {
    let d = b.sub(a);
    let m = d.x.abs().max(d.y.abs());
    a.add(p(m * d.x.signum(), m * d.y.signum()))
}
