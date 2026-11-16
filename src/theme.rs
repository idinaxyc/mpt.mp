//! Цветовая палитра.
//!
//! Источник по порядку: файл из переменной `MPT_THEME`, затем `~/mpt.mp/theme.toml`,
//! иначе встроенная палитра. Формат файла — простые строки `ключ = "#rrggbb"`
//! (см. `plugins/theme.toml`); ключи совпадают с `colors.toml` из Omarchy, поэтому
//! его можно подключить симлинком. Никаких внешних программ не запускается.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{OnceLock, RwLock};

/// Цвет sRGB без прозрачности. Своя мелкая структура, чтобы палитра не зависела от GUI-библиотеки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const BLACK: Rgb = Rgb { r: 0, g: 0, b: 0 };

    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// Из числа вида `0xRRGGBB`.
    pub const fn from_hex(rgb: u32) -> Self {
        Rgb {
            r: (rgb >> 16) as u8,
            g: (rgb >> 8) as u8,
            b: rgb as u8,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub name: String,
    pub bg: Rgb,
    pub panel: Rgb,
    pub card: Rgb,
    pub card_hi: Rgb,
    pub border: Rgb,
    pub text: Rgb,
    pub muted: Rgb,
    pub accent: Rgb,
    pub accent2: Rgb,
    pub danger: Rgb,
    pub warn: Rgb,
}

impl Palette {
    fn everforest() -> Self {
        Self {
            name: "Everforest".into(),
            bg: Rgb::from_hex(0x181d20),
            panel: Rgb::from_hex(0x21272c),
            card: Rgb::from_hex(0x2d353b),
            card_hi: Rgb::from_hex(0x343f44),
            border: Rgb::from_hex(0x3d484d),
            text: Rgb::from_hex(0xd3c6aa),
            muted: Rgb::from_hex(0x9da9a0),
            accent: Rgb::from_hex(0x7fbbb3),
            accent2: Rgb::from_hex(0xd699b6),
            danger: Rgb::from_hex(0xe67e80),
            warn: Rgb::from_hex(0xdbbc7f),
        }
    }
}

/// Текущая палитра. Меняется на лету (`apply`), поэтому хранится как «утёкшая» ссылка:
/// `pal()` по-прежнему отдаёт `&'static Palette`, а замена стоит одну маленькую утечку.
static PALETTE: RwLock<Option<&'static Palette>> = RwLock::new(None);

/// Сколько раз за запуск можно сменить палитру: каждая смена оставляет в памяти
/// одну запись (см. `PALETTE`), а плагин мог бы перекрашивать тему в цикле.
const MAX_PALETTE_CHANGES: u32 = 512;
static PALETTE_CHANGES: AtomicU32 = AtomicU32::new(0);

fn default_palette() -> &'static Palette {
    static DEFAULT: OnceLock<Palette> = OnceLock::new();
    DEFAULT.get_or_init(Palette::everforest)
}

pub fn pal() -> &'static Palette {
    match PALETTE.read() {
        Ok(guard) => match *guard {
            Some(p) => p,
            None => default_palette(),
        },
        Err(_) => default_palette(),
    }
}

fn set_palette(palette: Palette) {
    let leaked: &'static Palette = Box::leak(Box::new(palette));
    if let Ok(mut guard) = PALETTE.write() {
        *guard = Some(leaked);
    }
}

pub fn init() {
    set_palette(load());
}

/// Ключи цветов в файле темы (совпадают с `colors.toml` из Omarchy) и поля палитры.
const COLOR_KEYS: [&str; 11] = [
    "darker_background",
    "dark_background",
    "background",
    "lighter_background",
    "selection",
    "foreground",
    "light_foreground",
    "accent",
    "magenta",
    "red",
    "yellow",
];

impl Palette {
    fn colors(&self) -> [Rgb; 11] {
        [
            self.bg,
            self.panel,
            self.card,
            self.card_hi,
            self.border,
            self.text,
            self.muted,
            self.accent,
            self.accent2,
            self.danger,
            self.warn,
        ]
    }

    fn set_color(&mut self, key: &str, color: Rgb) {
        match key {
            "darker_background" => self.bg = color,
            "dark_background" => self.panel = color,
            "background" => self.card = color,
            "lighter_background" => self.card_hi = color,
            "selection" => self.border = color,
            "foreground" => self.text = color,
            "light_foreground" => self.muted = color,
            "accent" => self.accent = color,
            "magenta" => self.accent2 = color,
            "red" => self.danger = color,
            "yellow" => self.warn = color,
            _ => {}
        }
    }
}

fn to_hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r, c.g, c.b)
}

/// Строго `#rrggbb`.
fn parse_hex_strict(value: &str) -> Option<Rgb> {
    let v = value.strip_prefix('#')?;
    if v.len() != 6 || !v.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let part = |i: usize| u8::from_str_radix(&v[i..i + 2], 16).ok();
    Some(Rgb::new(part(0)?, part(2)?, part(4)?))
}

/// Текущая тема для плагинов: имя и цвета по ключам файла темы.
pub fn export() -> (String, Vec<(&'static str, String)>) {
    let p = pal();
    let colors = COLOR_KEYS
        .iter()
        .zip(p.colors())
        .map(|(key, color)| (*key, to_hex(color)))
        .collect();
    (p.name.clone(), colors)
}

/// Применяет тему от плагина: проверяет значения, меняет палитру на лету и сохраняет
/// `~/mpt.mp/theme.toml` (если это не симлинк — его пользователь подключил сам).
pub fn apply(name: Option<&str>, colors: &[(String, String)]) -> Result<(), String> {
    let current = pal();
    let mut palette = current.clone();
    for (key, value) in colors {
        if !COLOR_KEYS.contains(&key.as_str()) {
            return Err(format!("unknown color key '{key}'"));
        }
        let color = parse_hex_strict(value)
            .ok_or_else(|| format!("'{value}': expected a color like #rrggbb"))?;
        palette.set_color(key, color);
    }
    if let Some(name) = name {
        // Перевод строки в имени сломал бы файл темы (и позволил бы подсунуть чужие ключи).
        let name: String = name.chars().filter(|c| !c.is_control()).collect();
        let name = name.trim();
        if !name.is_empty() && name.chars().count() <= 60 {
            palette.name = name.to_string();
        }
    }
    // Та же тема (плагин синхронизации шлёт её регулярно): ни утечки, ни записи на диск.
    if palette == *current {
        return Ok(());
    }
    if PALETTE_CHANGES.fetch_add(1, Ordering::Relaxed) >= MAX_PALETTE_CHANGES {
        return Err("too many theme changes in one session; restart the player".into());
    }

    let mut text = format!("name = \"{}\"\n", palette.name.replace('"', ""));
    for (key, color) in COLOR_KEYS.iter().zip(palette.colors()) {
        text.push_str(&format!("{key} = \"{}\"\n", to_hex(color)));
    }
    set_palette(palette);

    let path = crate::model::root_dir().join("theme.toml");
    let is_link = std::fs::symlink_metadata(&path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if !is_link {
        let _ = crate::model::write_atomic(&path, text.as_bytes());
    }
    Ok(())
}

/// Цвет из файла темы: `#rrggbb` или `rrggbb`. Негодное значение — `None`
/// (тогда остаётся встроенный цвет); чёрный `#000000` — обычный цвет.
fn parse_color(value: &str) -> Option<Rgb> {
    let value = value.trim();
    parse_hex_strict(value).or_else(|| parse_hex_strict(&format!("#{value}")))
}

fn theme_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("MPT_THEME") {
        if !path.is_empty() {
            paths.push(PathBuf::from(path));
        }
    }
    paths.push(crate::model::root_dir().join("theme.toml"));
    paths
}

fn load() -> Palette {
    for path in theme_candidates() {
        if let Some(palette) = parse_file(&path) {
            return palette;
        }
    }
    Palette::everforest()
}

fn parse_file(path: &std::path::Path) -> Option<Palette> {
    let text = std::fs::read_to_string(path).ok()?;
    // Редакторы Windows любят ставить BOM в начало файла.
    let text = text.trim_start_matches('\u{feff}');
    let mut map = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().trim_matches('"').to_string();
            let value = unquote(value);
            map.insert(key, value);
        }
    }
    if map.is_empty() {
        return None;
    }

    let base = Palette::everforest();
    let get = |key: &str, fallback: Rgb| -> Rgb {
        map.get(key)
            .and_then(|v| parse_color(v))
            .unwrap_or(fallback)
    };

    let name = map
        .get("name")
        .cloned()
        .or_else(|| path.file_stem().map(|s| pretty_name(&s.to_string_lossy())))
        .unwrap_or_else(|| "Custom".into());

    Some(Palette {
        name,
        bg: get("darker_background", base.bg),
        panel: get("dark_background", base.panel),
        card: get("background", base.card),
        card_hi: get("lighter_background", base.card_hi),
        border: get("selection", base.border),
        text: get("foreground", base.text),
        muted: get("light_foreground", base.muted),
        accent: get("accent", base.accent),
        accent2: get("magenta", base.accent2),
        danger: get("red", base.danger),
        warn: get("yellow", base.warn),
    })
}

fn pretty_name(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Значение без кавычек и без хвостового комментария: `"#aabbcc"  # заметка` → `#aabbcc`.
fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote) {
            return rest.split(quote).next().unwrap_or("").to_string();
        }
    }
    value.split(" #").next().unwrap_or("").trim().to_string()
}
