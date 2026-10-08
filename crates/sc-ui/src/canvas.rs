use std::collections::HashMap;
use std::ffi::c_void;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits, GetObjectW,
    ReleaseDC,
};
use windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES;
use windows::Win32::UI::Shell::{SHFILEINFOW, SHGFI_ICON, SHGFI_LARGEICON, SHGetFileInfoW};
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO};
use windows::core::{PCWSTR, Result, w};
use windows_numerics::Vector2;

use crate::{Theme, wide};

/// Rectangle in device-independent pixels.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }
    pub fn right(self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(self) -> f32 {
        self.y + self.h
    }
    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    /// Shrinks horizontally by `d` on each side.
    pub fn pad_x(self, d: f32) -> Self {
        Self { x: self.x + d, w: (self.w - 2.0 * d).max(0.0), ..self }
    }
    /// Shrinks by `d` on every side.
    pub fn inset(self, d: f32) -> Self {
        Self { x: self.x + d, y: self.y + d, w: (self.w - 2.0 * d).max(0.0), h: (self.h - 2.0 * d).max(0.0) }
    }
    fn d2d(self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.right(), bottom: self.bottom() }
    }
}

/// Opaque colour as 0xRRGGBB.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Color(pub u32);

impl Color {
    fn d2d(self, a: f32) -> D2D1_COLOR_F {
        let ch = |shift: u32| ((self.0 >> shift) & 0xFF) as f32 / 255.0;
        D2D1_COLOR_F { r: ch(16), g: ch(8), b: ch(0), a }
    }

    /// Linear blend towards `other`; `t` = 0 keeps `self`, 1 gives `other`.
    pub fn mix(self, other: Color, t: f32) -> Color {
        let t = t.clamp(0.0, 1.0);
        let ch = |shift: u32| {
            let a = ((self.0 >> shift) & 0xFF) as f32;
            let b = ((other.0 >> shift) & 0xFF) as f32;
            ((a + (b - a) * t).round() as u32) << shift
        };
        Color(ch(16) | ch(8) | ch(0))
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Weight {
    Regular,
    Semi,
    Bold,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Font {
    pub size: f32,
    pub weight: Weight,
}

impl Font {
    pub const LABEL: Font = Font { size: 12.0, weight: Weight::Semi };
    pub const SMALL: Font = Font { size: 12.0, weight: Weight::Regular };
    pub const BODY: Font = Font { size: 13.0, weight: Weight::Regular };
    pub const BODY_BOLD: Font = Font { size: 13.0, weight: Weight::Semi };
    pub const TITLE: Font = Font { size: 14.0, weight: Weight::Bold };
    pub const BRAND: Font = Font { size: 15.0, weight: Weight::Bold };
    pub const HEADING: Font = Font { size: 17.0, weight: Weight::Bold };
    pub const TINY: Font = Font { size: 11.0, weight: Weight::Regular };
    pub const TILE: Font = Font { size: 16.0, weight: Weight::Bold };
    pub const VALUE: Font = Font { size: 18.0, weight: Weight::Bold };
    pub const DISPLAY: Font = Font { size: 20.0, weight: Weight::Bold };
    pub const BIG: Font = Font { size: 28.0, weight: Weight::Bold };
}

pub struct Canvas {
    pub(crate) rt: ID2D1HwndRenderTarget,
    brush: ID2D1SolidColorBrush,
    d2d: ID2D1Factory,
    round_caps: ID2D1StrokeStyle,
    dwrite: IDWriteFactory,
    formats: Vec<(Font, IDWriteTextFormat)>,
    // File icons by path; `None` records a file that has none. Bitmaps belong
    // to the render target, so the cache lives and dies with the canvas.
    icons: HashMap<String, Option<ID2D1Bitmap>>,
    wbuf: Vec<u16>,
    pub theme: Theme,
}

impl Canvas {
    pub(crate) fn new(
        d2d: &ID2D1Factory,
        dwrite: &IDWriteFactory,
        hwnd: HWND,
        px: (u32, u32),
        dpi: f32,
        theme: Theme,
    ) -> Result<Self> {
        unsafe {
            // Software rasterization: this UI repaints about once a second, and
            // a hardware target costs ~35 MB of private memory plus a pool of
            // driver threads for no visible gain.
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                dpiX: dpi,
                dpiY: dpi,
                ..Default::default()
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U { width: px.0, height: px.1 },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let rt = d2d.CreateHwndRenderTarget(&props, &hwnd_props)?;
            let brush = rt.CreateSolidColorBrush(&Color(0).d2d(1.0), None)?;
            let caps = D2D1_STROKE_STYLE_PROPERTIES {
                startCap: D2D1_CAP_STYLE_ROUND,
                endCap: D2D1_CAP_STYLE_ROUND,
                lineJoin: D2D1_LINE_JOIN_ROUND,
                ..Default::default()
            };
            let round_caps = d2d.CreateStrokeStyle(&caps, None)?;
            Ok(Self {
                rt,
                brush,
                d2d: d2d.clone(),
                round_caps,
                dwrite: dwrite.clone(),
                formats: Vec::new(),
                icons: HashMap::new(),
                wbuf: Vec::with_capacity(256),
                theme,
            })
        }
    }

    pub(crate) fn resize(&mut self, px: (u32, u32)) {
        unsafe {
            let _ = self.rt.Resize(&D2D_SIZE_U { width: px.0, height: px.1 });
        }
    }

    pub(crate) fn set_dpi(&mut self, dpi: f32) {
        unsafe { self.rt.SetDpi(dpi, dpi) };
    }

    fn format(&mut self, font: Font) -> Option<IDWriteTextFormat> {
        if let Some((_, f)) = self.formats.iter().find(|(k, _)| *k == font) {
            return Some(f.clone());
        }
        let weight = match font.weight {
            Weight::Regular => DWRITE_FONT_WEIGHT_NORMAL,
            Weight::Semi => DWRITE_FONT_WEIGHT_SEMI_BOLD,
            Weight::Bold => DWRITE_FONT_WEIGHT_BOLD,
        };
        let f = unsafe {
            let f = self
                .dwrite
                .CreateTextFormat(
                    w!("Segoe UI"),
                    None,
                    weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    font.size,
                    w!("en-us"),
                )
                .ok()?;
            let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            let trimming = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, ..Default::default() };
            if let Ok(ellipsis) = self.dwrite.CreateEllipsisTrimmingSign(&f) {
                let _ = f.SetTrimming(&trimming, &ellipsis);
            }
            f
        };
        self.formats.push((font, f.clone()));
        Some(f)
    }

    pub fn clear(&self, c: Color) {
        unsafe { self.rt.Clear(Some(&c.d2d(1.0))) };
    }

    pub fn fill(&self, r: Rect, c: Color) {
        unsafe {
            self.brush.SetColor(&c.d2d(1.0));
            self.rt.FillRectangle(&r.d2d(), &self.brush);
        }
    }

    pub fn fill_round(&self, r: Rect, radius: f32, c: Color) {
        unsafe {
            self.brush.SetColor(&c.d2d(1.0));
            let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
            self.rt.FillRoundedRectangle(&rr, &self.brush);
        }
    }

    pub fn stroke_round(&self, r: Rect, radius: f32, c: Color) {
        unsafe {
            self.brush.SetColor(&c.d2d(1.0));
            // Half-pixel inset keeps a 1 px stroke crisp.
            let r = Rect::new(r.x + 0.5, r.y + 0.5, r.w - 1.0, r.h - 1.0);
            let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
            self.rt.DrawRoundedRectangle(&rr, &self.brush, 1.0, None);
        }
    }

    /// Fully rounded rectangle.
    pub fn pill(&self, r: Rect, c: Color) {
        self.fill_round(r, r.h / 2.0, c);
    }

    /// [`Canvas::fill`] blended over what is already there; `alpha` 0..=1.
    pub fn fill_a(&self, r: Rect, c: Color, alpha: f32) {
        unsafe {
            self.brush.SetColor(&c.d2d(alpha));
            self.rt.FillRectangle(&r.d2d(), &self.brush);
        }
    }

    /// [`Canvas::fill_round`] blended over what is already there.
    pub fn fill_round_a(&self, r: Rect, radius: f32, c: Color, alpha: f32) {
        unsafe {
            self.brush.SetColor(&c.d2d(alpha));
            let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
            self.rt.FillRoundedRectangle(&rr, &self.brush);
        }
    }

    /// [`Canvas::stroke_round`] blended over what is already there.
    pub fn stroke_round_a(&self, r: Rect, radius: f32, c: Color, alpha: f32) {
        unsafe {
            self.brush.SetColor(&c.d2d(alpha));
            let r = Rect::new(r.x + 0.5, r.y + 0.5, r.w - 1.0, r.h - 1.0);
            let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
            self.rt.DrawRoundedRectangle(&rr, &self.brush, 1.0, None);
        }
    }

    /// Soft elliptical light centred on (`cx`, `cy`) that fades to nothing
    /// at its radii.
    pub fn glow(&self, cx: f32, cy: f32, rx: f32, ry: f32, c: Color, alpha: f32) {
        unsafe {
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: c.d2d(alpha) },
                D2D1_GRADIENT_STOP { position: 1.0, color: c.d2d(0.0) },
            ];
            let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                center: Vector2 { X: cx, Y: cy },
                gradientOriginOffset: Vector2 { X: 0.0, Y: 0.0 },
                radiusX: rx,
                radiusY: ry,
            };
            let brush = self
                .rt
                .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
                .and_then(|s| self.rt.CreateRadialGradientBrush(&props, None, &s));
            if let Ok(brush) = brush {
                self.rt.FillRectangle(&Rect::new(cx - rx, cy - ry, 2.0 * rx, 2.0 * ry).d2d(), &brush);
            }
        }
    }

    /// The window's backdrop: the ground colour lit warmly from two corners.
    /// It is what gives the glass panels something to sit over.
    pub fn backdrop(&self, w: f32, h: f32) {
        let t = self.theme;
        self.glow(w * 0.82, -h * 0.06, w * 0.62, h * 0.62, t.peach, 0.20);
        self.glow(w * 0.04, h * 1.04, w * 0.54, h * 0.60, t.butter, 0.13);
        self.glow(w * 0.46, h * 0.52, w * 0.44, h * 0.48, t.milk, 0.05);
    }

    /// A glass panel: a faint milk tint over the backdrop, a bright rim, and
    /// light catching the top edge. Nothing sharp is ever behind a panel, so
    /// no blur is needed to sell it.
    pub fn glass(&self, r: Rect, radius: f32) {
        let t = self.theme;
        self.fill_round_a(r, radius, t.milk, 0.055);
        self.stroke_round_a(r, radius, t.milk_hi, 0.10);
        self.fill_a(Rect::new(r.x + radius, r.y + 1.0, (r.w - 2.0 * radius).max(0.0), 1.0), t.milk_hi, 0.09);
    }

    /// A panel lifted off the page while it is being moved. Solid, so the
    /// panels it passes over do not show through it.
    pub fn glass_lifted(&self, r: Rect, radius: f32) {
        let t = self.theme;
        // Stand-in for a soft drop shadow.
        for (grow, alpha) in [(14.0, 0.10), (8.0, 0.14), (3.0, 0.18)] {
            let shadow = Rect::new(r.x - grow, r.y - grow + 12.0, r.w + 2.0 * grow, r.h + 2.0 * grow);
            self.fill_round_a(shadow, radius + grow, Color(0), alpha);
        }
        self.fill_round(r, radius, t.card.mix(t.milk, 0.06));
        self.stroke_round_a(r, radius, t.milk_hi, 0.32);
    }

    /// Recessed dark area inside a glass panel, such as a graph's background.
    pub fn well(&self, r: Rect, radius: f32) {
        let t = self.theme;
        self.fill_round_a(r, radius, t.well, 0.55);
        self.stroke_round_a(r, radius, t.milk_hi, 0.05);
    }

    /// The "milky" button body: a pill with a vertical gradient and a darker
    /// lip along the bottom so it reads as pressable.
    pub fn button_body(&self, r: Rect, top: Color, bottom: Color) {
        let lip = (r.h * 0.08).clamp(2.0, 3.0);
        self.pill(r, bottom.mix(Color(0), 0.16));
        let face = Rect::new(r.x, r.y, r.w, r.h - lip);
        if top == bottom {
            self.pill(face, top);
            return;
        }
        unsafe {
            let stops = [
                D2D1_GRADIENT_STOP { position: 0.0, color: top.d2d(1.0) },
                D2D1_GRADIENT_STOP { position: 1.0, color: bottom.d2d(1.0) },
            ];
            let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                startPoint: Vector2 { X: 0.0, Y: face.y },
                endPoint: Vector2 { X: 0.0, Y: face.bottom() },
            };
            let brush = self
                .rt
                .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
                .and_then(|s| self.rt.CreateLinearGradientBrush(&props, None, &s));
            match brush {
                Ok(brush) => {
                    let rr = D2D1_ROUNDED_RECT { rect: face.d2d(), radiusX: face.h / 2.0, radiusY: face.h / 2.0 };
                    self.rt.FillRoundedRectangle(&rr, &brush);
                }
                Err(_) => self.pill(face, bottom),
            }
        }
    }

    /// Line with round ends.
    pub fn line(&self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        unsafe {
            self.brush.SetColor(&c.d2d(1.0));
            self.rt.DrawLine(Vector2 { X: x0, Y: y0 }, Vector2 { X: x1, Y: y1 }, &self.brush, width, &self.round_caps);
        }
    }

    pub fn dot(&self, cx: f32, cy: f32, radius: f32, c: Color) {
        unsafe {
            self.brush.SetColor(&c.d2d(1.0));
            let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius };
            self.rt.FillEllipse(&e, &self.brush);
        }
    }

    pub fn ring(&self, cx: f32, cy: f32, radius: f32, width: f32, c: Color) {
        unsafe {
            self.brush.SetColor(&c.d2d(1.0));
            let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: radius, radiusY: radius };
            self.rt.DrawEllipse(&e, &self.brush, width, None);
        }
    }

    /// Line graph of `values` (each 0..=1, oldest first) inside `r`. The
    /// newest value sits at the right edge; `capacity` is how many values span
    /// the full width. With `fill`, the area under the line fades downwards.
    pub fn graph(&self, r: Rect, values: &[f32], capacity: usize, c: Color, width: f32, fill: bool) {
        if values.len() < 2 || capacity < 2 || r.w <= 0.0 || r.h <= 0.0 {
            return;
        }
        let step = r.w / (capacity - 1) as f32;
        let n = values.len().min(capacity);
        let values = &values[values.len() - n..];
        // Keep the stroke inside the rectangle at 0 and 1.
        let (top, span) = (r.y + width / 2.0, r.h - width);
        let points: Vec<Vector2> = values
            .iter()
            .enumerate()
            .map(|(i, v)| Vector2 {
                X: r.right() - (n - 1 - i) as f32 * step,
                Y: top + (1.0 - v.clamp(0.0, 1.0)) * span,
            })
            .collect();
        let path = |closed: bool| -> Result<ID2D1PathGeometry> {
            unsafe {
                let geo = self.d2d.CreatePathGeometry()?;
                let sink = geo.Open()?;
                if closed {
                    sink.BeginFigure(Vector2 { X: points[0].X, Y: r.bottom() }, D2D1_FIGURE_BEGIN_FILLED);
                    sink.AddLines(&points);
                    sink.AddLine(Vector2 { X: points[n - 1].X, Y: r.bottom() });
                    sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                } else {
                    sink.BeginFigure(points[0], D2D1_FIGURE_BEGIN_HOLLOW);
                    sink.AddLines(&points[1..]);
                    sink.EndFigure(D2D1_FIGURE_END_OPEN);
                }
                sink.Close()?;
                Ok(geo)
            }
        };
        unsafe {
            if fill && let Ok(area) = path(true) {
                let stops = [
                    D2D1_GRADIENT_STOP { position: 0.0, color: c.d2d(0.30) },
                    D2D1_GRADIENT_STOP { position: 1.0, color: c.d2d(0.0) },
                ];
                let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: Vector2 { X: 0.0, Y: r.y },
                    endPoint: Vector2 { X: 0.0, Y: r.bottom() },
                };
                let brush = self
                    .rt
                    .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
                    .and_then(|s| self.rt.CreateLinearGradientBrush(&props, None, &s));
                if let Ok(brush) = brush {
                    self.rt.FillGeometry(&area, &brush, None);
                }
            }
            if let Ok(line) = path(false) {
                self.brush.SetColor(&c.d2d(1.0));
                self.rt.DrawGeometry(&line, &self.brush, width, &self.round_caps);
            }
        }
    }

    pub fn clip(&self, r: Rect) {
        unsafe { self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_ALIASED) };
    }

    pub fn unclip(&self) {
        unsafe { self.rt.PopAxisAlignedClip() };
    }

    /// Body text; see [`Canvas::text_font`].
    pub fn text(&mut self, s: &str, r: Rect, c: Color, align: Align, bold: bool) {
        self.text_font(s, r, c, align, if bold { Font::BODY_BOLD } else { Font::BODY });
    }

    /// Single line, vertically centred in `r`, ellipsized when it does not fit.
    pub fn text_font(&mut self, s: &str, r: Rect, c: Color, align: Align, font: Font) {
        if s.is_empty() || r.w <= 1.0 {
            return;
        }
        let Some(format) = self.format(font) else { return };
        self.wbuf.clear();
        self.wbuf.extend(s.encode_utf16());
        unsafe {
            let _ = format.SetTextAlignment(match align {
                Align::Left => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::Right => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            self.brush.SetColor(&c.d2d(1.0));
            self.rt.DrawText(
                &self.wbuf,
                &format,
                &r.d2d(),
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }

    pub fn measure(&mut self, s: &str, bold: bool) -> f32 {
        self.measure_font(s, if bold { Font::BODY_BOLD } else { Font::BODY })
    }

    /// Width of `s` on one line.
    pub fn measure_font(&mut self, s: &str, font: Font) -> f32 {
        let Some(format) = self.format(font) else { return 0.0 };
        self.wbuf.clear();
        self.wbuf.extend(s.encode_utf16());
        unsafe {
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            let Ok(layout) = self.dwrite.CreateTextLayout(&self.wbuf, &format, 10_000.0, 100.0) else {
                return 0.0;
            };
            let mut m = DWRITE_TEXT_METRICS::default();
            match layout.GetMetrics(&mut m) {
                Ok(()) => m.widthIncludingTrailingWhitespace,
                Err(_) => 0.0,
            }
        }
    }

    /// Draws the shell icon of the file at `path` into `r`. Returns false when
    /// the file has no icon, so the caller can draw a stand-in.
    pub fn icon(&mut self, path: &str, r: Rect) -> bool {
        if !self.icons.contains_key(path) {
            let bitmap = self.load_icon(path);
            self.icons.insert(path.to_owned(), bitmap);
        }
        let Some(Some(bitmap)) = self.icons.get(path) else { return false };
        unsafe {
            self.rt.DrawBitmap(bitmap, Some(&r.d2d()), 1.0, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, None);
        }
        true
    }

    fn load_icon(&self, path: &str) -> Option<ID2D1Bitmap> {
        if path.is_empty() {
            return None;
        }
        let wpath = wide(path);
        unsafe {
            let mut info = SHFILEINFOW::default();
            let ok = SHGetFileInfoW(
                PCWSTR(wpath.as_ptr()),
                FILE_FLAGS_AND_ATTRIBUTES(0),
                Some(&mut info),
                size_of::<SHFILEINFOW>() as u32,
                SHGFI_ICON | SHGFI_LARGEICON,
            );
            if ok == 0 || info.hIcon.is_invalid() {
                return None;
            }
            let bitmap = self.bitmap_from_icon(info.hIcon);
            let _ = DestroyIcon(info.hIcon);
            bitmap
        }
    }

    unsafe fn bitmap_from_icon(&self, icon: HICON) -> Option<ID2D1Bitmap> {
        unsafe {
            let mut ii = ICONINFO::default();
            GetIconInfo(icon, &mut ii).ok()?;
            let result = (|| {
                let mut bm = BITMAP::default();
                if GetObjectW(ii.hbmColor.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut c_void)) == 0
                {
                    return None;
                }
                let (w, h) = (bm.bmWidth as u32, bm.bmHeight as u32);
                if w == 0 || h == 0 || w > 256 || h > 256 {
                    return None;
                }
                let mut bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w as i32,
                        // Negative height asks for top-down rows.
                        biHeight: -(h as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut px = vec![0u8; (w * h * 4) as usize];
                let dc = GetDC(None);
                let rows = GetDIBits(dc, ii.hbmColor, 0, h, Some(px.as_mut_ptr().cast()), &mut bmi, DIB_RGB_COLORS);
                ReleaseDC(None, dc);
                if rows == 0 {
                    return None;
                }
                // Icons from before Windows XP carry no alpha channel at all.
                let opaque = px.chunks_exact(4).all(|p| p[3] == 0);
                for p in px.chunks_exact_mut(4) {
                    let a = if opaque { 255 } else { p[3] as u32 };
                    for ch in &mut p[..3] {
                        *ch = (*ch as u32 * a / 255) as u8;
                    }
                    p[3] = a as u8;
                }
                let props = D2D1_BITMAP_PROPERTIES {
                    pixelFormat: D2D1_PIXEL_FORMAT {
                        format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                    },
                    dpiX: 96.0,
                    dpiY: 96.0,
                };
                self.rt
                    .CreateBitmap(D2D_SIZE_U { width: w, height: h }, Some(px.as_ptr().cast()), w * 4, &props)
                    .ok()
            })();
            let _ = DeleteObject(ii.hbmColor.into());
            let _ = DeleteObject(ii.hbmMask.into());
            result
        }
    }
}
