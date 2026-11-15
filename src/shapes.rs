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
    /// `ext` = measured layout size, filled in by the painter.
    Text {
        at: P,
        text: String,
        ext: Cell<(f32, f32)>,
    },
    Counter {
        at: P,
        n: u32,
    },
    Blur(P, P),
    Pixelate(P, P),
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
            Kind::Text { at, ext, .. } => {
                let (tw, th) = ext.get();
                let pad = self.text_pad();
                RectF::xywh(
                    at.x - pad,
                    at.y - pad,
                    tw.max(w * 0.4) + pad * 2.0,
                    th.max(w * 1.2) + pad * 2.0,
                )
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
            Kind::Blur(..) | Kind::Pixelate(..) | Kind::Text { .. } | Kind::Counter { .. } => {
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
