//! Text for the X11 bar: Figtree (Wispr's font, from nixpkgs) rasterised
//! with fontdue. X's core drawing cannot blend, so text is composited over the
//! colour under it (`Text::bg`) here.

use crate::bar::{over, Text};

pub struct Fonts {
    regular: fontdue::Font,
    bold: fontdue::Font,
}

impl Fonts {
    /// Figtree-Regular.ttf and Figtree-SemiBold.ttf from `dir`.
    pub fn load(dir: &str) -> Result<Fonts, String> {
        let font = |name: &str| {
            let path = format!("{dir}/{name}");
            let bytes = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
            fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default())
                .map_err(|e| format!("{path}: {e}"))
        };
        Ok(Fonts {
            regular: font("Figtree-Regular.ttf")?,
            bold: font("Figtree-SemiBold.ttf")?,
        })
    }

    fn font(&self, bold: bool) -> &fontdue::Font {
        if bold {
            &self.bold
        } else {
            &self.regular
        }
    }

    /// Advance width of `text` at `size` pixels.
    pub fn width(&self, text: &str, size: f64, bold: bool) -> f64 {
        let font = self.font(bold);
        let px = size as f32;
        let mut width = 0.0f32;
        let mut prev = None;
        for c in text.chars() {
            if let Some(p) = prev {
                width += font.horizontal_kern(p, c, px).unwrap_or(0.0);
            }
            width += font.metrics(c, px).advance_width;
            prev = Some(c);
        }
        f64::from(width)
    }

    /// `t` rendered at `scale` device pixels per CSS pixel: width, height and
    /// straight-alpha RGBA pixels, row by row.
    pub fn render(&self, t: &Text, scale: f64) -> (u16, u16, Vec<[u8; 4]>) {
        let font = self.font(t.bold);
        let px = (t.size * scale) as f32;
        let w = (self.width(&t.text, t.size, t.bold) * scale).ceil() as usize + 2;
        let h = (t.line * scale).round().max(1.0) as usize;
        let mut coverage = vec![0u8; w * h];
        let (ascent, descent) = font
            .horizontal_line_metrics(px)
            .map(|m| (m.ascent, m.descent))
            .unwrap_or((px * 0.8, -px * 0.2));
        let baseline = (h as f32 - (ascent - descent)) / 2.0 + ascent;
        let mut pen = 0.0f32;
        let mut prev = None;
        for c in t.text.chars() {
            if let Some(p) = prev {
                pen += font.horizontal_kern(p, c, px).unwrap_or(0.0);
            }
            let (m, bitmap) = font.rasterize(c, px);
            let left = (pen + m.xmin as f32).round() as isize;
            let top = (baseline - (m.height as f32 + m.ymin as f32)).round() as isize;
            for row in 0..m.height {
                for col in 0..m.width {
                    let (x, y) = (left + col as isize, top + row as isize);
                    if x < 0 || y < 0 || x as usize >= w || y as usize >= h {
                        continue;
                    }
                    let i = y as usize * w + x as usize;
                    coverage[i] = coverage[i].max(bitmap[row * m.width + col]);
                }
            }
            pen += m.advance_width;
            prev = Some(c);
        }
        let [r, g, b, a] = t.color;
        let pixels = coverage
            .iter()
            .map(|&cov| {
                let alpha = (u16::from(a) * u16::from(cov) / 255) as u8;
                over([r, g, b, alpha], t.bg)
            })
            .collect();
        (w as u16, h as u16, pixels)
    }
}
