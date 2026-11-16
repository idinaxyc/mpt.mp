//! Обложки: встроенные картинки из тегов + процедурная заглушка.

use crate::theme::Rgb;
use lofty::picture::PictureType;
use lofty::prelude::*;

/// Картинка в формате RGBA (альфа = 255): ровно то, что ждёт
/// `iced::widget::image::Handle::from_rgba`, без промежуточных копий.
#[derive(Clone, Debug)]
pub struct Picture {
    pub size: [usize; 2],
    pub rgba: Vec<u8>,
}

/// Максимальный размер стороны — больше для экрана не нужно.
const MAX_SIDE: u32 = 1600;

/// Достаёт встроенную обложку из аудиофайла и уменьшает её. Паника при разборе
/// повреждённого файла или картинки не должна убивать рабочий поток.
pub fn load(path: &str) -> Option<Picture> {
    std::panic::catch_unwind(|| load_inner(path)).ok().flatten()
}

fn load_inner(path: &str) -> Option<Picture> {
    let tagged = lofty::read_from_path(path).ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    // Лицевая обложка важнее фото исполнителя или задней стороны, даже если они в файле раньше.
    let picture = tag
        .pictures()
        .iter()
        .find(|p| matches!(p.pic_type(), PictureType::CoverFront))
        .or_else(|| tag.pictures().first())?;
    let bytes = picture.data();
    if bytes.is_empty() {
        return None;
    }
    let img = image::load_from_memory(bytes).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let img = if w.max(h) > MAX_SIDE {
        let scale = MAX_SIDE as f32 / w.max(h) as f32;
        img.resize(
            ((w as f32 * scale).round() as u32).max(1),
            ((h as f32 * scale).round() as u32).max(1),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let has_alpha = img.color().has_alpha();
    // into_rgba8 переиспользует буфер, если картинка уже RGBA.
    let buffer = img.into_rgba8();
    let size = [buffer.width() as usize, buffer.height() as usize];
    let mut rgba = buffer.into_raw();
    if has_alpha {
        // Прозрачные места (PNG) смешиваем с фоном панели, на месте.
        let back = crate::theme::pal().panel;
        let back = [back.r as u32, back.g as u32, back.b as u32];
        for p in rgba.chunks_exact_mut(4) {
            let a = p[3] as u32;
            if a == 255 {
                continue;
            }
            for k in 0..3 {
                p[k] = ((p[k] as u32 * a + back[k] * (255 - a) + 127) / 255) as u8;
            }
            p[3] = 255;
        }
    }
    Some(Picture { size, rgba })
}

/// Заглушка для треков без обложки: градиент, диагонали и «спектр».
pub fn fallback() -> Picture {
    let pal = crate::theme::pal();
    let side = 512usize;
    let mut px = vec![Rgb::BLACK; side * side];

    for y in 0..side {
        let t = y as f32 / side as f32;
        let row = mix(pal.panel, pal.bg, t);
        for x in 0..side {
            let mut c = row;
            if ((x + y) / 30) % 2 == 0 {
                c = mix(c, pal.card, 0.35);
            }
            px[y * side + x] = c;
        }
    }

    // Диагональная сетка тонких линий.
    for y in 0..side {
        for x in 0..side {
            if x % 64 == 0 || y % 64 == 0 {
                px[y * side + x] = mix(px[y * side + x], pal.border, 0.55);
            }
        }
    }

    // Пиксельный «спектр» внизу.
    let bars = 18usize;
    let gap = 6usize;
    let bar_w = (side - gap * (bars + 1)) / bars;
    for b in 0..bars {
        let phase = (b as f32 * 1.7).sin() * 0.5 + 0.5;
        let h = (60.0 + phase * 190.0) as usize;
        let x0 = gap + b * (bar_w + gap);
        let color = if b % 3 == 0 { pal.accent2 } else { pal.accent };
        for y in (side - 64 - h)..(side - 64) {
            for x in x0..(x0 + bar_w) {
                px[y * side + x] = mix(px[y * side + x], color, 0.72);
            }
        }
    }

    // Центральная рамка (везде толщиной 3 пикселя) и акцентный квадрат.
    let inner = 120usize;
    for i in inner..(side - inner) {
        for t in 0..3 {
            px[(inner + t) * side + i] = pal.accent;
            px[(side - inner - 1 - t) * side + i] = pal.accent;
            px[i * side + inner + t] = pal.accent;
            px[i * side + side - inner - 1 - t] = pal.accent;
        }
    }
    let sq = 46usize;
    for y in (side / 2 - sq)..(side / 2 + sq) {
        for x in (side / 2 - sq)..(side / 2 + sq) {
            px[y * side + x] = mix(px[y * side + x], pal.accent, 0.85);
        }
    }

    let mut rgba = Vec::with_capacity(px.len() * 4);
    for p in &px {
        rgba.extend_from_slice(&[p.r, p.g, p.b, 255]);
    }
    Picture {
        size: [side, side],
        rgba,
    }
}

/// Маленькая копия обложки (выборка ближайшего соседа).
pub fn thumb(image: &Picture, max_side: usize) -> Picture {
    let w = image.size[0].max(1);
    let h = image.size[1].max(1);
    let scale = max_side as f32 / w.max(h) as f32;
    let tw = ((w as f32 * scale).round() as usize).max(1);
    let th = ((h as f32 * scale).round() as usize).max(1);
    let mut rgba = Vec::with_capacity(tw * th * 4);
    for y in 0..th {
        let sy = (y * h / th).min(h - 1);
        let row = &image.rgba[sy * w * 4..(sy + 1) * w * 4];
        for x in 0..tw {
            let i = (x * w / tw).min(w - 1) * 4;
            rgba.extend_from_slice(&row[i..i + 4]);
        }
    }
    Picture {
        size: [tw, th],
        rgba,
    }
}

pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Rgb::new(f(a.r, b.r), f(a.g, b.g), f(a.b, b.b))
}
