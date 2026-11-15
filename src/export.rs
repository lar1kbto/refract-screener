//! Renders the selection with all annotations into a pixel buffer, 1:1 with the
//! physical screen pixels (no scaling, no resampling of the screenshot).

use crate::capture::Shot;
use crate::config::Tool;
use crate::effects;
use crate::encode::Image;
use crate::geom::RectI;
use crate::ocr::Block;
use crate::paint::*;
use crate::shapes::Shape;
use std::collections::HashMap;
use windows::core::Result;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;

/// Full-screen blurred / pixelated copies, keyed by (tool, strength).
#[derive(Default)]
pub struct FxCache {
    pub cpu: HashMap<(Tool, u32), Vec<u8>>,
    order: Vec<(Tool, u32)>,
}

impl FxCache {
    pub fn quantize(tool: Tool, s: u32) -> u32 {
        let s = s.max(2);
        if tool == Tool::Blur {
            (s + 1) / 2 * 2
        } else {
            s
        }
    }
    pub fn get(&mut self, shot: &Shot, tool: Tool, s: u32) -> &Vec<u8> {
        let key = (tool, Self::quantize(tool, s));
        if !self.cpu.contains_key(&key) {
            let (w, h) = (shot.w as usize, shot.h as usize);
            let data = if tool == Tool::Blur {
                effects::blur(&shot.px, w, h, key.1)
            } else {
                effects::pixelate(&shot.px, w, h, key.1)
            };
            self.cpu.insert(key, data);
            self.order.push(key);
            while self.order.len() > 6 {
                let old = self.order.remove(0);
                self.cpu.remove(&old);
            }
        }
        &self.cpu[&key]
    }
    pub fn evicted(&self, key: &(Tool, u32)) -> bool {
        !self.cpu.contains_key(key)
    }
}

pub fn bitmap_props() -> D2D1_BITMAP_PROPERTIES {
    D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
    }
}

pub fn make_bitmap(rt: &ID2D1RenderTarget, px: &[u8], w: i32, h: i32) -> Result<ID2D1Bitmap> {
    unsafe {
        rt.CreateBitmap(
            D2D_SIZE_U {
                width: w as u32,
                height: h as u32,
            },
            Some(px.as_ptr() as *const _),
            (w * 4) as u32,
            &bitmap_props(),
        )
    }
}

pub fn render(
    painter: &Painter,
    shot: &Shot,
    sel: RectI,
    shapes: &[Shape],
    fx: &mut FxCache,
    translation: Option<&[Block]>,
) -> Result<Image> {
    let sel = sel.clamp_to(&shot.full());
    let (w, h) = (sel.w().max(1) as u32, sel.h().max(1) as u32);

    // Fast path: nothing drawn, copy pixels directly.
    if shapes.is_empty() && translation.map_or(true, |t| t.iter().all(|b| b.translated.is_none())) {
        return Ok(Image {
            w,
            h,
            bgra: shot.crop(&sel),
        });
    }

    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let target =
            wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = painter.d2d.CreateWicBitmapRenderTarget(&target, &props)?;
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);

        // Only the selected part of the screenshot goes to the GPU.
        let crop = shot.crop(&sel);
        let base = make_bitmap(&rt, &crop, w as i32, h as i32)?;
        let mut bitmaps: HashMap<(Tool, u32), ID2D1Bitmap> = HashMap::new();

        rt.BeginDraw();
        rt.SetTransform(&Matrix3x2::identity());
        rt.DrawBitmap(
            &base,
            Some(&rc(crate::geom::RectF::new(0.0, 0.0, w as f32, h as f32))),
            1.0,
            D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            None,
        );
        rt.SetTransform(&Matrix3x2::translation(-sel.l as f32, -sel.t as f32));
        if let Some(blocks) = translation {
            painter.draw_translation(&rt, blocks);
        }
        let mut src = |tool: Tool, s: u32| -> Option<ID2D1Bitmap> {
            let key = (tool, FxCache::quantize(tool, s));
            if let Some(b) = bitmaps.get(&key) {
                return Some(b.clone());
            }
            let px = fx.get(shot, tool, s);
            let b = make_bitmap(&rt, px, shot.w, shot.h).ok()?;
            bitmaps.insert(key, b.clone());
            Some(b)
        };
        painter.draw_shapes(&rt, shapes, &mut src, None);
        rt.EndDraw(None, None)?;

        let mut bgra = vec![0u8; (w * h * 4) as usize];
        target.CopyPixels(std::ptr::null(), w * 4, &mut bgra)?;
        for a in bgra.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        Ok(Image { w, h, bgra })
    }
}
