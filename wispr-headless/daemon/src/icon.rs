//! Decode broker-supplied icons to a bounded, inert bitmap off the UI thread.
use base64::Engine;
#[cfg(target_os = "macos")]
use image::ImageEncoder;
use image::ImageReader;
use std::io::{Cursor, Read};

#[derive(Debug, PartialEq)]
pub struct Icon {
    pub pixels: image::RgbaImage,
    #[cfg(target_os = "macos")]
    pub png: Vec<u8>,
}

impl Icon {
    pub fn load(source: &str) -> Result<Self, String> {
        let bytes = if source.starts_with("data:") {
            if source.len() > dictation_api::MAX_ICON_BYTES {
                return Err("icon is too large".into());
            }
            let (header, data) = source.split_once(',').ok_or("invalid icon data URL")?;
            if header.ends_with(";base64") {
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| e.to_string())?
            } else {
                let mut bytes = Vec::new();
                let mut data = data.bytes();
                while let Some(b) = data.next() {
                    bytes.push(if b == b'%' {
                        let hi = data
                            .next()
                            .and_then(|b| (b as char).to_digit(16))
                            .ok_or("invalid URL escape")?;
                        let lo = data
                            .next()
                            .and_then(|b| (b as char).to_digit(16))
                            .ok_or("invalid URL escape")?;
                        (hi * 16 + lo) as u8
                    } else {
                        b
                    });
                }
                bytes
            }
        } else {
            let path = std::path::Path::new(source);
            if !path.is_absolute() {
                return Err("icon path must be absolute".into());
            }
            if !path.metadata().map_err(|e| e.to_string())?.is_file() {
                return Err("icon is not a regular file".into());
            }
            let mut bytes = Vec::new();
            std::fs::File::open(path)
                .map_err(|e| e.to_string())?
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| e.to_string())?;
            if bytes.len() > 1024 * 1024 {
                return Err("icon file is too large".into());
            }
            bytes
        };
        let pixels =
            if let Ok(mut reader) = ImageReader::new(Cursor::new(&bytes)).with_guessed_format() {
                let mut limits = image::Limits::default();
                limits.max_image_width = Some(2048);
                limits.max_image_height = Some(2048);
                limits.max_alloc = Some(32 * 1024 * 1024);
                reader.limits(limits);
                reader.decode().ok().map(|img| {
                    img.resize(64, 64, image::imageops::FilterType::Lanczos3)
                        .to_rgba8()
                })
            } else {
                None
            };
        let pixels = match pixels {
            Some(pixels) => pixels,
            None => {
                let mut options = resvg::usvg::Options::default();
                // SVG is parsed as drawing data. Never load external resources.
                options.image_href_resolver.resolve_string = Box::new(|_, _| None);
                let tree =
                    resvg::usvg::Tree::from_data(&bytes, &options).map_err(|e| e.to_string())?;
                let size = tree.size();
                let scale = (64.0 / size.width()).min(64.0 / size.height());
                let mut pixmap =
                    resvg::tiny_skia::Pixmap::new(64, 64).ok_or("icon allocation failed")?;
                resvg::render(
                    &tree,
                    resvg::tiny_skia::Transform::from_scale(scale, scale),
                    &mut pixmap.as_mut(),
                );
                let mut rgba = pixmap.take();
                // tiny-skia uses premultiplied RGBA, image uses straight alpha.
                for pixel in rgba.chunks_exact_mut(4) {
                    if pixel[3] != 0 {
                        for i in 0..3 {
                            pixel[i] =
                                (u32::from(pixel[i]) * 255 / u32::from(pixel[3])).min(255) as u8;
                        }
                    }
                }
                image::RgbaImage::from_raw(64, 64, rgba).ok_or("invalid icon bitmap")?
            }
        };
        #[cfg(target_os = "macos")]
        let png = {
            let mut bytes = Vec::new();
            image::codecs::png::PngEncoder::new(&mut bytes)
                .write_image(
                    pixels.as_raw(),
                    pixels.width(),
                    pixels.height(),
                    image::ExtendedColorType::Rgba8,
                )
                .map_err(|e| e.to_string())?;
            bytes
        };
        Ok(Self {
            pixels,
            #[cfg(target_os = "macos")]
            png,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raster_and_svg_icons_become_bounded_bitmaps() {
        let svg = "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='10000' height='10000'%3E%3Cscript%3Ethrow 1%3C/script%3E%3Crect width='10000' height='10000' fill='red'/%3E%3C/svg%3E";
        let icon = Icon::load(svg).unwrap();
        assert_eq!(icon.pixels.dimensions(), (64, 64));
        assert_eq!(icon.pixels.get_pixel(32, 32).0, [255, 0, 0, 255]);
        let file = tempfile::NamedTempFile::new().unwrap();
        icon.pixels
            .save_with_format(file.path(), image::ImageFormat::Png)
            .unwrap();
        let raster = Icon::load(file.path().to_str().unwrap()).unwrap();
        assert_eq!(raster.pixels, icon.pixels);
        assert!(Icon::load("relative.png").is_err());
        assert!(Icon::load("https://example.com/icon.png").is_err());
        assert!(Icon::load("data:image/png;base64,bad").is_err());
        assert!(Icon::load(&format!(
            "data:image/png;base64,{}",
            "A".repeat(dictation_api::MAX_ICON_BYTES)
        ))
        .is_err());
    }

    #[test]
    fn svg_does_not_resolve_external_images() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("private.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([255, 0, 0, 255]))
            .save(&path)
            .unwrap();
        let svg = format!("data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' width='4' height='4'><image href='{}' width='4' height='4'/></svg>", path.display());
        let icon = Icon::load(&svg).unwrap();
        assert!(icon.pixels.pixels().all(|p| p.0 == [0; 4]));
    }
}
