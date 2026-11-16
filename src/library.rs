use crate::model::{short_id, Track, AUDIO_EXTS};
use lofty::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub fn is_audio(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

pub fn make_track(path: &Path) -> Track {
    let path_str = path.to_string_lossy().to_string();
    let meta = std::fs::metadata(path).ok();
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let mtime = meta.as_ref().map(mtime_secs).unwrap_or(0);

    let mut title = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Unknown".into());
    let mut artist = String::new();
    let mut album = String::new();
    let mut duration_ms = 0u64;

    // Битый файл не должен ронять поток сканирования: на испорченных контейнерах
    // разбор тегов иногда паникует, тогда трек остаётся с названием из имени файла.
    let tagged = std::panic::catch_unwind(|| lofty::read_from_path(path))
        .ok()
        .and_then(Result::ok);
    if let Some(tagged) = tagged {
        duration_ms = tagged.properties().duration().as_millis() as u64;
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(t) = tag.title().filter(|s| !s.trim().is_empty()) {
                title = t.to_string();
            }
            if let Some(a) = tag.artist().filter(|s| !s.trim().is_empty()) {
                artist = a.to_string();
            }
            if let Some(al) = tag.album().filter(|s| !s.trim().is_empty()) {
                album = al.to_string();
            }
        }
    }

    Track {
        id: short_id(&format!("{path_str}:{size}")),
        path: path_str,
        title,
        artist,
        album,
        duration_ms,
        size,
        mtime,
    }
}

fn mtime_secs(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Читает теги нескольких файлов в нескольких потоках (до 4).
fn read_tracks(paths: &[PathBuf]) -> Vec<Track> {
    if paths.len() < 16 {
        return paths.iter().map(|p| make_track(p)).collect();
    }
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2)
        .clamp(2, 4);
    let chunk = paths.len().div_ceil(threads);
    std::thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk)
            .map(|part| scope.spawn(move || part.iter().map(|p| make_track(p)).collect::<Vec<_>>()))
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    })
}

/// Сканирует папки и возвращает найденные треки (без дублей по пути).
///
/// `known` — треки из прошлого скана: если путь, размер и время изменения файла
/// те же, теги не перечитываются. На большой библиотеке это разница между
/// секундами и долей секунды при каждом запуске.
pub fn scan(folders: &[String], known: &[Track], excluded: &[String]) -> Vec<Track> {
    let excluded: std::collections::HashSet<&str> = excluded.iter().map(String::as_str).collect();
    let cache: HashMap<&str, &Track> = known.iter().map(|t| (t.path.as_str(), t)).collect();

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut out: Vec<Track> = Vec::new();
    let mut todo: Vec<PathBuf> = Vec::new();
    // Папки, которые сейчас недоступны (не примонтирован диск, сетевая папка).
    let mut offline: Vec<&Path> = Vec::new();

    for folder in folders {
        let root = Path::new(folder);
        if !root.is_dir() {
            offline.push(root);
            continue;
        }
        for entry in WalkDir::new(root).follow_links(true).into_iter().flatten() {
            if !entry.file_type().is_file() || !is_audio(entry.path()) {
                continue;
            }
            let path = entry.path().to_string_lossy().into_owned();
            if excluded.contains(path.as_str()) || seen.contains(&path) {
                continue;
            }
            let (size, mtime) = entry
                .metadata()
                .ok()
                .map(|m| (m.len(), mtime_secs(&m)))
                .unwrap_or((0, 0));
            match cache.get(path.as_str()) {
                Some(t) if mtime != 0 && t.mtime == mtime && t.size == size => {
                    out.push((*t).clone());
                }
                _ => todo.push(entry.into_path()),
            }
            seen.insert(path);
        }
    }
    // Недоступная папка не значит «музыки больше нет»: оставляем её прежние треки,
    // иначе плейлисты потеряли бы их до следующего скана.
    if !offline.is_empty() {
        for t in known {
            if !seen.contains(t.path.as_str())
                && !excluded.contains(t.path.as_str())
                && offline
                    .iter()
                    .any(|root| Path::new(&t.path).starts_with(root))
            {
                out.push(t.clone());
            }
        }
    }
    let mut fresh = read_tracks(&todo);
    // Файл изменился (например, после правки тегов) — id остаётся прежним,
    // иначе трек пропал бы из плейлистов.
    for t in &mut fresh {
        if let Some(old) = cache.get(t.path.as_str()) {
            t.id = old.id.clone();
        }
    }
    out.extend(fresh);

    // Ключ считается один раз на трек; в `sort_by` каждое сравнение делало бы
    // шесть `to_lowercase()` (шесть аллокаций).
    out.sort_by_cached_key(|t| {
        (
            t.artist.to_lowercase(),
            t.album.to_lowercase(),
            t.title.to_lowercase(),
        )
    });
    out
}
