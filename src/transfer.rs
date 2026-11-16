use crate::model::{Playlist, Track};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// Пределы при распаковке архива (защита от «zip-бомб»): на один файл и на весь архив.
const MAX_ENTRY_BYTES: u64 = 4 << 30;
const MAX_TOTAL_BYTES: u64 = 32 << 30;
/// Плейлист внутри архива читается не дальше этого размера.
const MAX_M3U_BYTES: u64 = 8 << 20;

pub fn m3u_text(tracks: &[Track]) -> String {
    let mut out = String::from("#EXTM3U\n");
    for t in tracks {
        append_extinf(&mut out, t);
        out.push_str(&t.path);
        out.push('\n');
    }
    out
}

/// Строка `#EXTINF`. Название и исполнитель — в одну строку: перевод строки
/// в теге сломал бы файл (следующая строка читалась бы как путь).
fn append_extinf(output: &mut String, track: &Track) {
    let one_line = |s: &str| s.replace(['\r', '\n'], " ");
    output.push_str("#EXTINF:");
    output.push_str(&(track.duration_ms / 1000).to_string());
    output.push(',');
    if !track.artist.is_empty() {
        output.push_str(&one_line(&track.artist));
        output.push_str(" - ");
    }
    output.push_str(&one_line(&track.title));
    output.push('\n');
}

pub fn export_m3u(path: &Path, tracks: &[Track]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, m3u_text(tracks))?;
    Ok(())
}

/// Читает .m3u/.m3u8 и возвращает пути из него (относительные считаются от папки файла).
pub fn read_m3u(path: &Path) -> Result<Vec<PathBuf>> {
    // Старые .m3u бывают не в UTF-8 — читаем «с заменой», а не падаем.
    let bytes = std::fs::read(path).context("cannot read m3u")?;
    let text = String::from_utf8_lossy(&bytes);
    let base = path.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Плейлисты из Windows: «Music\a\b.mp3» → «Music/a/b.mp3».
        let line = if line.contains('\\') && !line.contains('/') {
            line.replace('\\', "/")
        } else {
            line.to_string()
        };
        let p = PathBuf::from(line);
        let resolved = if p.is_absolute() { p } else { base.join(p) };
        out.push(resolved);
    }
    Ok(out)
}

pub fn export_zip(
    path: &Path,
    playlist: &Playlist,
    tracks: &[Track],
    include_audio: bool,
) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = File::create(path).context("cannot create zip")?;
    let mut zip = ZipWriter::new(file);
    let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    let m3u_name = format!("{}.m3u", safe_stem(&playlist.name));
    // Файлы, которых уже нет на диске, не попадают ни в архив, ни в список.
    let mut used_names = HashSet::with_capacity(tracks.len());
    let entries: Vec<(&Track, String)> = tracks
        .iter()
        .filter(|track| !include_audio || Path::new(&track.path).is_file())
        .map(|track| {
            let name = unique_archive_name(&sanitize(&track.file_name()), &mut used_names);
            (track, name)
        })
        .collect();

    let mut m3u = String::from("#EXTM3U\n");
    for (track, archive_name) in &entries {
        append_extinf(&mut m3u, track);
        if include_audio {
            m3u.push_str("tracks/");
            m3u.push_str(archive_name);
        } else {
            m3u.push_str(&track.path);
        }
        m3u.push('\n');
    }
    zip.start_file(&m3u_name, opts)?;
    zip.write_all(m3u.as_bytes())?;

    if include_audio {
        for (track, archive_name) in &entries {
            let Ok(mut file) = File::open(&track.path) else {
                continue;
            };
            zip.start_file(format!("tracks/{archive_name}"), stored)?;
            // Потоком, без загрузки всего файла в память.
            std::io::copy(&mut file, &mut zip)?;
        }
    }
    zip.finish()?;
    Ok(())
}

/// Распаковывает zip с плейлистом в imports/<имя-архива>/
/// и возвращает (имя плейлиста, пути к найденным аудиофайлам).
pub fn import_zip(path: &Path) -> Result<(String, Vec<PathBuf>)> {
    let file = File::open(path).context("cannot open zip")?;
    let mut archive = ZipArchive::new(file).context("corrupt zip")?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "import".into());
    let dest = crate::model::imports_dir().join(safe_stem(&stem));
    std::fs::create_dir_all(&dest)?;

    let mut m3u_text: Option<String> = None;
    let mut playlist_name = stem.clone();
    let mut extracted: Vec<PathBuf> = Vec::new();
    let mut total: u64 = 0;

    for i in 0..archive.len() {
        let Ok(mut entry) = archive.by_index(i) else {
            continue;
        };
        if entry.is_dir() {
            continue;
        }
        let raw_name = entry.name().to_string();
        let file_name = raw_name.rsplit('/').next().unwrap_or(&raw_name).to_string();
        // Служебные файлы macOS («__MACOSX/._song.mp3») выглядят как аудио, но не аудио.
        if raw_name.starts_with("__MACOSX/") || file_name.starts_with("._") {
            continue;
        }
        let lower = file_name.to_ascii_lowercase();

        if lower.ends_with(".m3u") || lower.ends_with(".m3u8") {
            // Старые плейлисты бывают не в UTF-8: читаем «с заменой» и не больше предела.
            let mut bytes = Vec::new();
            let _ = entry.by_ref().take(MAX_M3U_BYTES).read_to_end(&mut bytes);
            let ext_len = if lower.ends_with(".m3u8") { 5 } else { 4 };
            let name = file_name[..file_name.len() - ext_len].trim();
            if !name.is_empty() {
                playlist_name = name.to_string();
            }
            m3u_text = Some(String::from_utf8_lossy(&bytes).into_owned());
            continue;
        }
        if !crate::library::is_audio(Path::new(&file_name)) {
            continue;
        }
        // Файл с тем же именем не перезаписывается; пишем потоком на диск, а не в память.
        let target = unique_target(&dest, &sanitize(&file_name));
        let Ok(mut output) = File::create(&target) else {
            continue;
        };
        let limit = MAX_ENTRY_BYTES.min(MAX_TOTAL_BYTES.saturating_sub(total));
        match std::io::copy(&mut entry.by_ref().take(limit), &mut output) {
            Ok(n) if n < limit => {
                total += n;
                extracted.push(target);
            }
            result => {
                // Ошибка или превышен предел: недописанный файл не оставляем.
                drop(output);
                let _ = std::fs::remove_file(&target);
                if result.is_ok() {
                    break;
                }
            }
        }
    }

    let ordered = match m3u_text {
        Some(text) => order_by_playlist(extracted, &text),
        None => extracted,
    };
    Ok((playlist_name, ordered))
}

/// Если в m3u есть порядок — извлечённые файлы идут по нему, остальные — следом.
/// Индекс по имени строится один раз: поиск в цикле был бы квадратичным.
fn order_by_playlist(files: Vec<PathBuf>, m3u: &str) -> Vec<PathBuf> {
    let by_name: HashMap<String, usize> = files
        .iter()
        .enumerate()
        .filter_map(|(i, p)| Some((p.file_name()?.to_string_lossy().into_owned(), i)))
        .collect();
    let mut taken = vec![false; files.len()];
    let mut order: Vec<usize> = Vec::with_capacity(files.len());
    for line in m3u.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let name = line.rsplit(['/', '\\']).next().unwrap_or(line);
        if let Some(&i) = by_name.get(name) {
            if !taken[i] {
                taken[i] = true;
                order.push(i);
            }
        }
    }
    order.extend((0..files.len()).filter(|&i| !taken[i]));
    let mut slots: Vec<Option<PathBuf>> = files.into_iter().map(Some).collect();
    order.into_iter().filter_map(|i| slots[i].take()).collect()
}

/// «имя (N).расширение» — для номеров N ≥ 2.
fn numbered_name(name: &str, index: u32) -> String {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("track");
    match Path::new(name).extension().and_then(|s| s.to_str()) {
        Some(ext) => format!("{stem} ({index}).{ext}"),
        None => format!("{stem} ({index})"),
    }
}

fn unique_archive_name(name: &str, used: &mut HashSet<String>) -> String {
    if used.insert(name.to_ascii_lowercase()) {
        return name.to_string();
    }
    for index in 2u32.. {
        let candidate = numbered_name(name, index);
        if used.insert(candidate.to_ascii_lowercase()) {
            return candidate;
        }
    }
    unreachable!("u32 filename suffix space exhausted")
}

pub fn safe_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_string();
    if trimmed.is_empty() {
        "playlist".into()
    } else {
        trimmed
    }
}

/// Возвращает свободный путь, не перезаписывая ранее импортированный файл.
pub fn unique_target(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    for index in 2u32.. {
        let candidate = dir.join(numbered_name(name, index));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("u32 filename suffix space exhausted")
}

fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_control()
                || matches!(
                    c,
                    '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            {
                '_'
            } else {
                c
            }
        })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() {
        "track.mp3".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mptmp-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("Music")).unwrap();
        std::env::set_var("HOME", &dir);
        crate::model::ensure_dirs();
        dir
    }

    fn wav(path: &Path, seconds: u32) -> Track {
        let rate: u32 = 8000;
        let data_len = rate * seconds * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        bytes.resize(44 + data_len as usize, 0);
        std::fs::write(path, &bytes).unwrap();
        Track {
            id: crate::model::short_id(&path.to_string_lossy()),
            path: path.to_string_lossy().to_string(),
            title: path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            artist: "Тест".into(),
            album: "Альбом".into(),
            duration_ms: seconds as u64 * 1000,
            size: bytes.len() as u64,
            mtime: 0,
        }
    }

    fn demo(tag: &str) -> (Vec<Track>, Playlist) {
        let home = setup(tag);
        let dir = home.join("Music");
        let first = wav(&dir.join("01 one.wav"), 1);
        let second = wav(&dir.join("02 два.wav"), 2);
        let playlist = Playlist {
            id: "pl-test".into(),
            name: "Мой плейлист".into(),
            track_ids: vec![first.id.clone(), second.id.clone()],
        };
        (vec![first, second], playlist)
    }

    #[test]
    fn extinf_stays_on_one_line() {
        let track = Track {
            id: "x".into(),
            path: "/a/b.mp3".into(),
            title: "two\nlines".into(),
            artist: "a\r\nb".into(),
            album: String::new(),
            duration_ms: 3000,
            size: 1,
            mtime: 0,
        };
        assert_eq!(
            m3u_text(&[track]),
            "#EXTM3U\n#EXTINF:3,a  b - two lines\n/a/b.mp3\n"
        );
    }

    #[test]
    fn m3u_round_trip() {
        let (tracks, _playlist) = demo("m3u");
        let out = crate::model::exports_dir().join("Мой плейлист.m3u");
        export_m3u(&out, &tracks).unwrap();

        let text = std::fs::read_to_string(&out).unwrap();
        assert!(text.starts_with("#EXTM3U\n"));
        assert!(text.contains("#EXTINF:1,Тест - 01 one"));
        assert!(text.contains("#EXTINF:2,Тест - 02 два"));

        let files = read_m3u(&out).unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0], PathBuf::from(&tracks[0].path));
        assert_eq!(files[1], PathBuf::from(&tracks[1].path));
    }

    #[test]
    fn zip_with_audio_round_trip() {
        let (tracks, playlist) = demo("zip-audio");
        let out = crate::model::exports_dir().join("pl.zip");
        export_zip(&out, &playlist, &tracks, true).unwrap();
        assert!(out.is_file());

        let (name, files) = import_zip(&out).unwrap();
        assert_eq!(name, "Мой плейлист");
        assert_eq!(files.len(), 2);
        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["01 one.wav", "02 два.wav"]);
        for (extracted, original) in files.iter().zip(tracks.iter()) {
            let a = std::fs::read(extracted).unwrap();
            let b = std::fs::read(&original.path).unwrap();
            assert_eq!(a.len(), b.len());
            assert_eq!(a, b);
        }
    }

    #[test]
    fn zip_without_audio_keeps_paths() {
        let (tracks, playlist) = demo("zip-links");
        let out = crate::model::exports_dir().join("links.zip");
        export_zip(&out, &playlist, &tracks, false).unwrap();

        let (name, files) = import_zip(&out).unwrap();
        assert_eq!(name, "Мой плейлист");
        assert!(files.is_empty(), "без аудио распаковывать нечего");
        assert!(out.with_file_name("links.zip").is_file());
    }

    #[test]
    fn safe_stem_cleans_names() {
        assert_eq!(safe_stem("a/b:c*?\"<>|d"), "a_b_c______d");
        assert_eq!(safe_stem("   "), "playlist");
        assert_eq!(safe_stem("..hidden.."), "hidden");
    }

    #[test]
    fn zip_names_are_unique_even_on_case_insensitive_filesystems() {
        let mut used = HashSet::new();
        assert_eq!(unique_archive_name("song.mp3", &mut used), "song.mp3");
        assert_eq!(unique_archive_name("song.mp3", &mut used), "song (2).mp3");
        assert_eq!(unique_archive_name("SONG.MP3", &mut used), "SONG (3).MP3");
    }

    #[test]
    fn unique_target_does_not_overwrite_existing_files() {
        let dir = std::env::temp_dir().join(format!("mpt-unique-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("track.mp3"), b"first").unwrap();

        let second = unique_target(&dir, "track.mp3");
        assert_eq!(
            second.file_name().unwrap().to_string_lossy(),
            "track (2).mp3"
        );
        std::fs::write(&second, b"second").unwrap();
        assert_eq!(std::fs::read(dir.join("track.mp3")).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
