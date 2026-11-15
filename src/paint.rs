//! Direct2D drawing of annotations. The same code renders the live overlay and the
//! exported image, so what you see is exactly what gets saved.

use crate::config::{Fill, Rgb, Tool};
use crate::geom::*;
use crate::ocr::Block;
use crate::shapes::{Kind, Shape};
use windows::core::Result;
use windows::Win32::Foundation::BOOL;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;

pub fn col(c: Rgb, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.r(),
        g: c.g(),
        b: c.b(),
        a,
    }
}
pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F { r, g, b, a }
}
pub fn pt(q: P) -> D2D_POINT_2F {
    D2D_POINT_2F { x: q.x, y: q.y }
}
pub fn rc(r: RectF) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.l,
        top: r.t,
        right: r.r,
        bottom: r.b,
    }
}
pub fn rrect(r: RectF, rad: f32) -> D2D1_ROUNDED_RECT {
    let rad = rad.min(r.w() * 0.5).min(r.h() * 0.5).max(0.0);
    D2D1_ROUNDED_RECT {
        rect: rc(r),
        radiusX: rad,
        radiusY: rad,
    }
}

pub fn contrast(c: Rgb) -> Rgb {
    if c.luma() > 0.6 {
        Rgb(0x111318)
    } else {
        Rgb(0xFFFFFF)
    }
}

pub struct Painter {
    pub d2d: ID2D1Factory,
    pub dw: IDWriteFactory,
    pub round: ID2D1StrokeStyle,
    pub font: Vec<u16>,
    pub radius: f32,
}

/// Returns the full-screen blurred / pixelated bitmap for (tool, strength).
pub type FxSource<'a> = &'a mut dyn FnMut(Tool, u32) -> Option<ID2D1Bitmap>;

impl Painter {
    pub fn new(font: &str, radius: f32) -> Result<Painter> {
        unsafe {
            let opts = D2D1_FACTORY_OPTIONS::default();
            let d2d: ID2D1Factory =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, Some(&opts))?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let props = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                dashCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                miterLimit: 10.0,
                dashStyle: D2D1_DASH_STYLE_SOLID,
                dashOffset: 0.0,
            };
            let round = d2d.CreateStrokeStyle(&props, None)?;
            Ok(Painter {
                d2d,
                dw,
                round,
                font: crate::win::wide(font),
                radius,
            })
        }
    }

    pub fn dashed(&self) -> Result<ID2D1StrokeStyle> {
        let props = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: D2D1_CAP_STYLE_FLAT,
            endCap: D2D1_CAP_STYLE_FLAT,
            dashCap: D2D1_CAP_STYLE_FLAT,
            lineJoin: D2D1_LINE_JOIN_MITER,
            miterLimit: 10.0,
            dashStyle: D2D1_DASH_STYLE_CUSTOM,
            dashOffset: 0.0,
        };
        unsafe { self.d2d.CreateStrokeStyle(&props, Some(&[4.0, 3.0])) }
    }

    pub fn brush(&self, rt: &ID2D1RenderTarget, c: D2D1_COLOR_F) -> Result<ID2D1SolidColorBrush> {
        unsafe { rt.CreateSolidColorBrush(&c, None) }
    }

    pub fn format(&self, size: f32, weight: DWRITE_FONT_WEIGHT) -> Result<IDWriteTextFormat> {
        unsafe {
            self.dw.CreateTextFormat(
                windows::core::PCWSTR(self.font.as_ptr()),
                None,
                weight,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size.max(1.0),
                windows::core::w!("en-us"),
            )
        }
    }

    pub fn layout(
        &self,
        text: &str,
        f: &IDWriteTextFormat,
        maxw: f32,
        maxh: f32,
    ) -> Result<IDWriteTextLayout> {
        let w: Vec<u16> = text.encode_utf16().collect();
        unsafe {
            self.dw
                .CreateTextLayout(&w, f, maxw.max(1.0), maxh.max(1.0))
        }
    }

    pub fn metrics(l: &IDWriteTextLayout) -> (f32, f32) {
        let mut m = DWRITE_TEXT_METRICS::default();
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        (m.widthIncludingTrailingWhitespace.max(m.width), m.height)
    }

    fn path(&self, build: impl FnOnce(&ID2D1GeometrySink)) -> Result<ID2D1PathGeometry> {
        unsafe {
            let g = self.d2d.CreatePathGeometry()?;
            let s = g.Open()?;
            s.SetFillMode(D2D1_FILL_MODE_WINDING);
            build(&s);
            s.Close()?;
            Ok(g)
        }
    }

    /// Smooth freehand stroke: quadratic curves through the midpoints.
    fn stroke_path(&self, pts: &[P]) -> Result<ID2D1PathGeometry> {
        self.path(|s| unsafe {
            s.BeginFigure(pt(pts[0]), D2D1_FIGURE_BEGIN_HOLLOW);
            if pts.len() == 1 {
                s.AddLine(pt(pts[0].add(p(0.01, 0.0))));
            } else if pts.len() == 2 {
                s.AddLine(pt(pts[1]));
            } else {
                for i in 1..pts.len() - 1 {
                    let mid = pts[i].lerp(pts[i + 1], 0.5);
                    s.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT {
                        point1: pt(pts[i]),
                        point2: pt(mid),
                    });
                }
                s.AddLine(pt(*pts.last().unwrap()));
            }
            s.EndFigure(D2D1_FIGURE_END_OPEN);
        })
    }

    /// Tapered arrow with a swept-back head.
    pub fn arrow_path(&self, a: P, b: P, w: f32) -> Result<ID2D1PathGeometry> {
        let len = a.dist(b).max(0.01);
        let u = b.sub(a).norm();
        let n = u.perp();
        let mut hl = w * 3.4 + 9.0;
        let mut hw = w * 2.0 + 5.0;
        if hl > len * 0.6 {
            let k = (len * 0.6) / hl;
            hl *= k;
            hw *= k;
        }
        let t0 = (w * 0.22).max(0.6);
        let t1 = (w * 0.55).max(1.0);
        let base = b.sub(u.mul(hl));
        let neck = base.add(u.mul(hl * 0.22));
        let pts = [
            neck.add(n.mul(t1)),
            base.add(n.mul(hw)),
            b,
            base.sub(n.mul(hw)),
            neck.sub(n.mul(t1)),
            a.sub(n.mul(t0)),
        ];
        let start = a.add(n.mul(t0));
        self.path(|s| unsafe {
            s.BeginFigure(pt(start), D2D1_FIGURE_BEGIN_FILLED);
            let v: Vec<D2D_POINT_2F> = pts.iter().map(|q| pt(*q)).collect();
            s.AddLines(&v);
            s.EndFigure(D2D1_FIGURE_END_CLOSED);
        })
    }

    pub fn draw_shapes(
        &self,
        rt: &ID2D1RenderTarget,
        shapes: &[Shape],
        fx: FxSource,
        caret: Option<usize>,
    ) {
        for (i, s) in shapes.iter().enumerate() {
            let _ = self.draw_shape(rt, s, fx, caret == Some(i));
        }
    }

    pub fn draw_shape(
        &self,
        rt: &ID2D1RenderTarget,
        s: &Shape,
        fx: FxSource,
        caret: bool,
    ) -> Result<()> {
        if s.style.shadow && !matches!(s.kind, Kind::Blur(..) | Kind::Pixelate(..)) {
            let k = (s.style.width * 0.12).clamp(1.0, 4.0);
            for (dx, dy, a) in [(k * 0.6, k * 1.6, 0.16), (k * 0.25, k * 0.8, 0.18)] {
                self.with_offset(rt, dx, dy, |rt| {
                    self.pass(rt, s, fx, Some(a * s.style.opacity), false)
                })?;
            }
        }
        self.pass(rt, s, fx, None, caret)
    }

    fn with_offset(
        &self,
        rt: &ID2D1RenderTarget,
        dx: f32,
        dy: f32,
        f: impl FnOnce(&ID2D1RenderTarget) -> Result<()>,
    ) -> Result<()> {
        unsafe {
            let mut m = windows::Foundation::Numerics::Matrix3x2::default();
            rt.GetTransform(&mut m);
            let mut s = m;
            s.M31 += dx;
            s.M32 += dy;
            rt.SetTransform(&s);
            let r = f(rt);
            rt.SetTransform(&m);
            r
        }
    }

    /// One rendering pass. `shadow` = Some(alpha) draws the silhouette in black.
    fn pass(
        &self,
        rt: &ID2D1RenderTarget,
        s: &Shape,
        fx: FxSource,
        shadow: Option<f32>,
        caret: bool,
    ) -> Result<()> {
        let st = s.style;
        let w = st.width;
        let main = match shadow {
            Some(a) => rgba(0.0, 0.0, 0.0, a),
            None => col(st.color, st.opacity),
        };
        let br = self.brush(rt, main)?;
        unsafe {
            match &s.kind {
                Kind::Pen(pts) | Kind::Marker(pts) => {
                    if pts.is_empty() {
                        return Ok(());
                    }
                    let g = self.stroke_path(pts)?;
                    rt.DrawGeometry(&g, &br, w, &self.round);
                }
                Kind::Line(a, b) => rt.DrawLine(pt(*a), pt(*b), &br, w, &self.round),
                Kind::Arrow(a, b) => {
                    let g = self.arrow_path(*a, *b, w)?;
                    rt.FillGeometry(&g, &br, None);
                    rt.DrawGeometry(&g, &br, (w * 0.18).clamp(0.6, 2.0), &self.round);
                }
                Kind::Rect(a, b) => {
                    let r = RectF::from_pts(*a, *b);
                    let rr = rrect(r, self.radius);
                    match (st.fill, shadow) {
                        (Fill::Soft, None) => rt.FillRoundedRectangle(
                            &rr,
                            &self.brush(rt, col(st.color, st.opacity * 0.22))?,
                        ),
                        (Fill::Solid, _) | (Fill::Soft, Some(_)) => {
                            rt.FillRoundedRectangle(&rr, &br)
                        }
                        _ => {}
                    }
                    rt.DrawRoundedRectangle(&rr, &br, w, &self.round);
                }
                Kind::Ellipse(a, b) => {
                    let r = RectF::from_pts(*a, *b);
                    let e = D2D1_ELLIPSE {
                        point: D2D_POINT_2F {
                            x: r.cx(),
                            y: r.cy(),
                        },
                        radiusX: r.w() * 0.5,
                        radiusY: r.h() * 0.5,
                    };
                    match (st.fill, shadow) {
                        (Fill::Soft, None) => {
                            rt.FillEllipse(&e, &self.brush(rt, col(st.color, st.opacity * 0.22))?)
                        }
                        (Fill::Solid, _) | (Fill::Soft, Some(_)) => rt.FillEllipse(&e, &br),
                        _ => {}
                    }
                    rt.DrawEllipse(&e, &br, w, &self.round);
                }
                Kind::Blur(a, b) | Kind::Pixelate(a, b) => {
                    let r = RectF::from_pts(*a, *b);
                    let r = RectF::new(r.l.round(), r.t.round(), r.r.round(), r.b.round());
                    let tool = s.tool();
                    match fx(tool, w.round() as u32) {
                        Some(bmp) => {
                            let mode = if tool == Tool::Pixelate {
                                D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR
                            } else {
                                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR
                            };
                            rt.DrawBitmap(&bmp, Some(&rc(r)), 1.0, mode, Some(&rc(r)));
                        }
                        None => {
                            rt.FillRectangle(&rc(r), &self.brush(rt, rgba(0.5, 0.5, 0.5, 0.85))?)
                        }
                    }
                }
                Kind::Text { at, text, ext } => {
                    let f = self.format(w, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
                    let l = self.layout(text, &f, 8000.0, 8000.0)?;
                    let (tw, th) = Self::metrics(&l);
                    let th = th.max(w * 1.25);
                    ext.set((tw, th));
                    let pad = s.text_pad();
                    let bx = RectF::xywh(
                        at.x - pad,
                        at.y - pad,
                        tw.max(w * 0.3) + pad * 2.0,
                        th + pad * 2.0,
                    );
                    let rad = (pad * 1.2).min(self.radius.max(4.0));
                    let fg = match (st.fill, shadow) {
                        (_, Some(_)) => {
                            if st.fill != Fill::None {
                                rt.FillRoundedRectangle(&rrect(bx, rad), &br);
                                return Ok(());
                            }
                            main
                        }
                        (Fill::None, None) => main,
                        (Fill::Soft, None) => {
                            rt.FillRoundedRectangle(
                                &rrect(bx, rad),
                                &self.brush(rt, rgba(0.07, 0.075, 0.09, 0.78 * st.opacity))?,
                            );
                            main
                        }
                        (Fill::Solid, None) => {
                            rt.FillRoundedRectangle(&rrect(bx, rad), &br);
                            col(contrast(st.color), st.opacity)
                        }
                    };
                    let fb = self.brush(rt, fg)?;
                    rt.DrawTextLayout(pt(*at), &l, &fb, D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT);
                    if caret && shadow.is_none() {
                        let (mut x, mut y) = (0.0f32, 0.0f32);
                        let mut m = DWRITE_HIT_TEST_METRICS::default();
                        let n = text.encode_utf16().count() as u32;
                        let _ = if n == 0 {
                            l.HitTestTextPosition(0, BOOL(0), &mut x, &mut y, &mut m)
                        } else {
                            l.HitTestTextPosition(n - 1, BOOL(1), &mut x, &mut y, &mut m)
                        };
                        let h = if m.height > 0.0 { m.height } else { w * 1.25 };
                        rt.DrawLine(
                            pt(p(at.x + x + 1.0, at.y + y + 1.0)),
                            pt(p(at.x + x + 1.0, at.y + y + h - 1.0)),
                            &fb,
                            (w * 0.07).max(1.5),
                            &self.round,
                        );
                    }
                }
                Kind::Counter { at, n } => {
                    let r = s.counter_radius();
                    let e = D2D1_ELLIPSE {
                        point: pt(*at),
                        radiusX: r,
                        radiusY: r,
                    };
                    if shadow.is_some() {
                        rt.FillEllipse(&e, &br);
                        return Ok(());
                    }
                    let num = match st.fill {
                        Fill::Solid => {
                            rt.FillEllipse(&e, &br);
                            col(contrast(st.color), st.opacity)
                        }
                        Fill::Soft => {
                            rt.FillEllipse(
                                &e,
                                &self.brush(rt, rgba(0.07, 0.075, 0.09, 0.8 * st.opacity))?,
                            );
                            rt.DrawEllipse(&e, &br, (r * 0.13).max(1.5), &self.round);
                            main
                        }
                        Fill::None => {
                            rt.DrawEllipse(&e, &br, (r * 0.13).max(1.5), &self.round);
                            main
                        }
                    };
                    let f = self.format(
                        r * if *n >= 10 { 0.95 } else { 1.15 },
                        DWRITE_FONT_WEIGHT_BOLD,
                    )?;
                    let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
                    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                    let l = self.layout(&n.to_string(), &f, r * 2.0, r * 2.0)?;
                    rt.DrawTextLayout(
                        pt(p(at.x - r, at.y - r * 1.04)),
                        &l,
                        &self.brush(rt, num)?,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            }
        }
        Ok(())
    }

    /// Paints translated text over the recognised text, like a camera translator.
    pub fn draw_translation(&self, rt: &ID2D1RenderTarget, blocks: &[Block]) {
        for b in blocks {
            let Some(text) = b.translated.as_ref() else {
                continue;
            };
            let _ = (|| -> Result<()> {
                let bg = self.brush(rt, col(b.bg, 1.0))?;
                let pad = (b.line_h * 0.14).max(1.5);
                unsafe {
                    for r in &b.line_rects {
                        rt.FillRoundedRectangle(&rrect(r.inflate(pad), pad * 1.5), &bg);
                    }
                    rt.FillRectangle(&rc(b.rect.inflate(pad * 0.5)), &bg);
                }
                let maxw = b.rect.w().max(b.line_h * 2.0) + pad * 2.0;
                let mut size = b.fitted.get();
                if size <= 0.0 {
                    size = (b.line_h * 1.02).max(7.0);
                    for _ in 0..24 {
                        let f = self.format(size, DWRITE_FONT_WEIGHT_NORMAL)?;
                        let l = self.layout(text, &f, maxw, 10000.0)?;
                        let (_, h) = Self::metrics(&l);
                        if h <= b.rect.h() * 1.12 + pad * 2.0 || size <= 7.0 {
                            break;
                        }
                        size = (size * 0.92).max(7.0);
                    }
                    b.fitted.set(size);
                }
                let f = self.format(size, DWRITE_FONT_WEIGHT_NORMAL)?;
                let l = self.layout(text, &f, maxw, 10000.0)?;
                let (_, h) = Self::metrics(&l);
                let y = b.rect.t + ((b.rect.h() - h) * 0.5).max(-pad);
                let fg = self.brush(rt, col(b.fg, 1.0))?;
                unsafe {
                    rt.DrawTextLayout(
                        pt(p(b.rect.l - pad * 0.5, y)),
                        &l,
                        &fg,
                        D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
                    );
                }
                Ok(())
            })();
        }
    }
}
