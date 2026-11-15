//! Small geometry helpers shared by the overlay, painter and export.

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct P {
    pub x: f32,
    pub y: f32,
}

pub const fn p(x: f32, y: f32) -> P {
    P { x, y }
}

impl P {
    pub fn add(self, o: P) -> P {
        p(self.x + o.x, self.y + o.y)
    }
    pub fn sub(self, o: P) -> P {
        p(self.x - o.x, self.y - o.y)
    }
    pub fn mul(self, k: f32) -> P {
        p(self.x * k, self.y * k)
    }
    pub fn len(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }
    pub fn dist(self, o: P) -> f32 {
        self.sub(o).len()
    }
    pub fn norm(self) -> P {
        let l = self.len();
        if l < 1e-6 {
            p(1.0, 0.0)
        } else {
            self.mul(1.0 / l)
        }
    }
    pub fn perp(self) -> P {
        p(-self.y, self.x)
    }
    pub fn lerp(self, o: P, t: f32) -> P {
        p(self.x + (o.x - self.x) * t, self.y + (o.y - self.y) * t)
    }
}

/// Distance from point `q` to segment `a`-`b`.
pub fn seg_dist(q: P, a: P, b: P) -> f32 {
    let ab = b.sub(a);
    let l2 = ab.x * ab.x + ab.y * ab.y;
    if l2 < 1e-6 {
        return q.dist(a);
    }
    let t = (((q.x - a.x) * ab.x + (q.y - a.y) * ab.y) / l2).clamp(0.0, 1.0);
    q.dist(a.add(ab.mul(t)))
}

/// Float rectangle, always normalised (l <= r, t <= b) by constructors.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct RectF {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RectF {
    pub fn new(l: f32, t: f32, r: f32, b: f32) -> Self {
        RectF { l, t, r, b }
    }
    pub fn from_pts(a: P, b: P) -> Self {
        RectF::new(a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y))
    }
    pub fn xywh(x: f32, y: f32, w: f32, h: f32) -> Self {
        RectF::new(x, y, x + w, y + h)
    }
    pub fn w(&self) -> f32 {
        self.r - self.l
    }
    pub fn h(&self) -> f32 {
        self.b - self.t
    }
    pub fn cx(&self) -> f32 {
        (self.l + self.r) * 0.5
    }
    pub fn cy(&self) -> f32 {
        (self.t + self.b) * 0.5
    }
    pub fn contains(&self, q: P) -> bool {
        q.x >= self.l && q.x <= self.r && q.y >= self.t && q.y <= self.b
    }
    pub fn inflate(&self, d: f32) -> RectF {
        RectF::new(self.l - d, self.t - d, self.r + d, self.b + d)
    }
    pub fn union(&self, o: &RectF) -> RectF {
        RectF::new(
            self.l.min(o.l),
            self.t.min(o.t),
            self.r.max(o.r),
            self.b.max(o.b),
        )
    }
    pub fn translate(&self, dx: f32, dy: f32) -> RectF {
        RectF::new(self.l + dx, self.t + dy, self.r + dx, self.b + dy)
    }
}

/// Integer rectangle in image pixels (right/bottom exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct RectI {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl RectI {
    pub fn new(l: i32, t: i32, r: i32, b: i32) -> Self {
        RectI { l, t, r, b }
    }
    pub fn norm(a: (i32, i32), b: (i32, i32)) -> Self {
        RectI::new(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
    }
    pub fn w(&self) -> i32 {
        self.r - self.l
    }
    pub fn h(&self) -> i32 {
        self.b - self.t
    }
    pub fn is_empty(&self) -> bool {
        self.w() <= 0 || self.h() <= 0
    }
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    pub fn clamp_to(&self, o: &RectI) -> RectI {
        RectI::new(
            self.l.max(o.l),
            self.t.max(o.t),
            self.r.min(o.r),
            self.b.min(o.b),
        )
    }
    pub fn f(&self) -> RectF {
        RectF::new(self.l as f32, self.t as f32, self.r as f32, self.b as f32)
    }
}
