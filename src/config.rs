//! User settings, stored as JSON in %APPDATA%\Refract-screener\config.json.
//! Every field has a default, so a partial or older file still loads.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 0xRRGGBB colour, written to JSON as "#RRGGBB".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u32);

impl Rgb {
    pub fn r(self) -> f32 {
        ((self.0 >> 16) & 0xFF) as f32 / 255.0
    }
    pub fn g(self) -> f32 {
        ((self.0 >> 8) & 0xFF) as f32 / 255.0
    }
    pub fn b(self) -> f32 {
        (self.0 & 0xFF) as f32 / 255.0
    }
    pub fn luma(self) -> f32 {
        0.2126 * self.r() + 0.7152 * self.g() + 0.0722 * self.b()
    }
    pub fn from_f(r: f32, g: f32, b: f32) -> Rgb {
        let c = |v: f32| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u32;
        Rgb((c(r) << 16) | (c(g) << 8) | c(b))
    }
    pub fn hex(self) -> String {
        format!("#{:06X}", self.0 & 0xFFFFFF)
    }
    pub fn parse(s: &str) -> Option<Rgb> {
        let s = s.trim().trim_start_matches('#');
        let v = u32::from_str_radix(s, 16).ok()?;
        match s.len() {
            6 => Some(Rgb(v)),
            3 => {
                let (r, g, b) = ((v >> 8) & 0xF, (v >> 4) & 0xF, v & 0xF);
                Some(Rgb((r * 17) << 16 | (g * 17) << 8 | b * 17))
            }
            _ => None,
        }
    }
    /// HSV (h 0..360, s/v 0..1) to RGB.
    pub fn from_hsv(h: f32, s: f32, v: f32) -> Rgb {
        let h = (h.rem_euclid(360.0)) / 60.0;
        let c = v * s;
        let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
        let (r, g, b) = match h as i32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = v - c;
        Rgb::from_f(r + m, g + m, b + m)
    }
    pub fn to_hsv(self) -> (f32, f32, f32) {
        let (r, g, b) = (self.r(), self.g(), self.b());
        let mx = r.max(g).max(b);
        let mn = r.min(g).min(b);
        let d = mx - mn;
        let h = if d < 1e-6 {
            0.0
        } else if mx == r {
            60.0 * (((g - b) / d).rem_euclid(6.0))
        } else if mx == g {
            60.0 * ((b - r) / d + 2.0)
        } else {
            60.0 * ((r - g) / d + 4.0)
        };
        let s = if mx < 1e-6 { 0.0 } else { d / mx };
        (h, s, mx)
    }
}

impl Serialize for Rgb {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Rgb::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("bad colour {s}")))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize, PartialOrd, Ord)]
pub enum Tool {
    Select,
    Pen,
    Line,
    Arrow,
    Rect,
    Ellipse,
    Marker,
    Text,
    Counter,
    Blur,
    Pixelate,
}

impl Tool {
    pub const ALL: [Tool; 11] = [
        Tool::Select,
        Tool::Pen,
        Tool::Line,
        Tool::Arrow,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Marker,
        Tool::Text,
        Tool::Counter,
        Tool::Blur,
        Tool::Pixelate,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Tool::Select => "Select / Move (V)",
            Tool::Pen => "Pen (P)",
            Tool::Line => "Line (L)",
            Tool::Arrow => "Arrow (A)",
            Tool::Rect => "Rectangle (R)",
            Tool::Ellipse => "Ellipse (E)",
            Tool::Marker => "Highlighter (M)",
            Tool::Text => "Text (T)",
            Tool::Counter => "Step number (N)",
            Tool::Blur => "Blur (B)",
            Tool::Pixelate => "Pixelate (X)",
        }
    }
    pub fn key(self) -> u32 {
        (match self {
            Tool::Select => 'V',
            Tool::Pen => 'P',
            Tool::Line => 'L',
            Tool::Arrow => 'A',
            Tool::Rect => 'R',
            Tool::Ellipse => 'E',
            Tool::Marker => 'M',
            Tool::Text => 'T',
            Tool::Counter => 'N',
            Tool::Blur => 'B',
            Tool::Pixelate => 'X',
        }) as u32
    }
    /// What the size slider means for this tool.
    pub fn size_label(self) -> &'static str {
        match self {
            Tool::Text => "Font",
            Tool::Counter => "Size",
            Tool::Blur => "Radius",
            Tool::Pixelate => "Block",
            _ => "Width",
        }
    }
    pub fn size_range(self) -> (f32, f32) {
        match self {
            Tool::Text => (8.0, 120.0),
            Tool::Counter => (8.0, 64.0),
            Tool::Marker => (4.0, 60.0),
            Tool::Blur => (2.0, 40.0),
            Tool::Pixelate => (3.0, 40.0),
            _ => (1.0, 40.0),
        }
    }
    pub fn has_fill(self) -> bool {
        matches!(
            self,
            Tool::Rect | Tool::Ellipse | Tool::Text | Tool::Counter
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Fill {
    None,
    Soft,
    Solid,
}

impl Fill {
    pub fn next(self) -> Fill {
        match self {
            Fill::None => Fill::Soft,
            Fill::Soft => Fill::Solid,
            Fill::Solid => Fill::None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Style {
    pub color: Rgb,
    pub width: f32,
    pub opacity: f32,
    pub fill: Fill,
    pub shadow: bool,
}

impl Default for Style {
    fn default() -> Self {
        Style {
            color: Rgb(0xFF3B5C),
            width: 4.0,
            opacity: 1.0,
            fill: Fill::None,
            shadow: true,
        }
    }
}

pub fn default_style(t: Tool) -> Style {
    let d = Style::default();
    match t {
        Tool::Arrow => Style { width: 5.0, ..d },
        Tool::Marker => Style {
            color: Rgb(0xFFE14D),
            width: 18.0,
            opacity: 0.45,
            shadow: false,
            ..d
        },
        Tool::Text => Style { width: 24.0, ..d },
        Tool::Counter => Style {
            width: 18.0,
            fill: Fill::Solid,
            ..d
        },
        Tool::Blur => Style {
            width: 12.0,
            shadow: false,
            ..d
        },
        Tool::Pixelate => Style {
            width: 10.0,
            shadow: false,
            ..d
        },
        _ => d,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ImageFormat {
    /// Lossless PNG, optimised with oxipng (palette/bit-depth reduction + best deflate).
    Png,
    /// Lossless WebP (libwebp 1.6, method 5): about 30 % smaller than optimised PNG,
    /// up to 45 % on UI and text.
    WebP,
}

impl ImageFormat {
    pub fn ext(self) -> &'static str {
        match self {
            ImageFormat::Png => "png",
            ImageFormat::WebP => "webp",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Open the region overlay. Win+Shift+S and PrintScreen replace Snipping Tool.
    pub hotkey_region: Vec<String>,
    /// Instantly capture the monitor under the cursor (no overlay).
    pub hotkey_fullscreen: Vec<String>,
    /// Instantly capture the active window (no overlay).
    pub hotkey_window: Vec<String>,

    pub format: ImageFormat,
    /// oxipng effort 0..6 (higher = smaller file, slower). 6 also tries Zopfli-like effort.
    pub png_effort: u8,
    pub save_dir: String,
    /// Tokens: {yyyy} {MM} {dd} {HH} {mm} {ss} {ms} {w} {h}
    pub file_name: String,
    pub auto_save: bool,
    pub copy_to_clipboard: bool,
    pub notify: bool,

    pub capture_cursor: bool,
    pub magnifier: bool,
    pub snap_windows: bool,
    pub dim: f32,
    pub accent: Rgb,
    pub ui_scale: f32,
    pub font: String,
    pub corner_radius: f32,
    pub palette: Vec<Rgb>,
    pub delay_seconds: u32,

    /// Recognise text automatically as soon as a region is selected.
    pub auto_ocr: bool,
    /// Play the confirmation sound when a screenshot is saved.
    pub sound: bool,
    /// Check `update_url` for new versions on start and every 6 hours.
    pub auto_update: bool,
    /// PrintScreen and Win+Shift+S belong to Refract (a background guard keeps them
    /// from Snipping Tool even when the app is closed). Off = Windows gets them back.
    pub replace_snipping_tool: bool,
    pub update_url: String,
    /// BCP-47 tag like "en-US" or "ru"; empty = Windows profile languages.
    pub ocr_language: String,
    /// Google Cloud Translation API key. Empty = public Google Translate web endpoint.
    pub google_api_key: String,
    pub translate_to: String,
    pub translate_languages: Vec<String>,

    pub tool: Tool,
    pub styles: BTreeMap<Tool, Style>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            hotkey_region: vec!["PrintScreen".into(), "Win+Shift+S".into()],
            hotkey_fullscreen: vec!["Ctrl+PrintScreen".into()],
            hotkey_window: vec!["Alt+PrintScreen".into()],
            format: ImageFormat::Png,
            png_effort: 4,
            save_dir: String::new(),
            file_name: "Refract_{yyyy}-{MM}-{dd}_{HH}-{mm}-{ss}".into(),
            auto_save: true,
            copy_to_clipboard: true,
            notify: true,
            capture_cursor: false,
            magnifier: true,
            snap_windows: true,
            dim: 0.5,
            accent: Rgb(0x3FE0B5),
            ui_scale: 1.0,
            font: "Segoe UI".into(),
            corner_radius: 6.0,
            palette: [
                0xFF3B5C, 0xFF8A3D, 0xFFE14D, 0x3FE0B5, 0x34C3FF, 0x5B6CFF, 0xB45CFF, 0xFF5CD1,
                0xFFFFFF, 0x111318,
            ]
            .into_iter()
            .map(Rgb)
            .collect(),
            delay_seconds: 3,
            auto_ocr: true,
            sound: true,
            auto_update: true,
            replace_snipping_tool: true,
            update_url: option_env!("REFRACT_UPDATE_URL").unwrap_or("").into(),
            ocr_language: String::new(),
            google_api_key: String::new(),
            translate_to: "ru".into(),
            translate_languages: [
                "ru", "en", "uk", "de", "fr", "es", "it", "pl", "tr", "zh-CN", "ja", "ko",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            tool: Tool::Arrow,
            styles: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn style(&self, t: Tool) -> Style {
        self.styles
            .get(&t)
            .copied()
            .unwrap_or_else(|| default_style(t))
    }
    pub fn set_style(&mut self, t: Tool, s: Style) {
        self.styles.insert(t, s);
    }
}

pub fn app_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Refract-screener")
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.json")
}

pub struct Loaded {
    pub cfg: Config,
    /// Set when the file exists but could not be parsed: we never overwrite it then.
    pub error: Option<String>,
}

pub fn load() -> Loaded {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<Config>(&text) {
            Ok(mut cfg) => {
                sanitize(&mut cfg);
                Loaded { cfg, error: None }
            }
            Err(e) => Loaded {
                cfg: Config::default(),
                error: Some(format!("{}: {e}", path.display())),
            },
        },
        Err(_) => {
            let cfg = Config::default();
            save(&cfg);
            Loaded { cfg, error: None }
        }
    }
}

fn sanitize(c: &mut Config) {
    c.png_effort = c.png_effort.min(6);
    c.dim = c.dim.clamp(0.0, 0.95);
    c.ui_scale = c.ui_scale.clamp(0.6, 3.0);
    c.corner_radius = c.corner_radius.clamp(0.0, 40.0);
    if c.palette.is_empty() {
        c.palette = Config::default().palette;
    }
    c.palette.truncate(16);
    if c.translate_languages.is_empty() {
        c.translate_languages = Config::default().translate_languages;
    }
    for (t, s) in c.styles.iter_mut() {
        let (lo, hi) = t.size_range();
        s.width = s.width.clamp(lo, hi);
        s.opacity = s.opacity.clamp(0.05, 1.0);
    }
}

pub fn save(cfg: &Config) {
    let dir = app_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(text) = serde_json::to_string_pretty(cfg) {
        let tmp = dir.join("config.json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, config_path());
        }
    }
}
