//! Общий слой приложения, не зависящий от интерфейса: библиотека, плеер, настройки
//! и хост плагинов. Интерфейс (сейчас iced) только показывает состояние и шлёт сюда команды.

use crate::model::{self, short_id, LibraryState, Playlist, Track};
use crate::player::{Player, RepeatMode};
use crate::plugins::{required_permission, ApiResult, Handled, Host, Incoming, Request};
use crate::settings::Settings;
use crate::transfer;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// Через сколько после первой несохранённой правки библиотека пишется на диск.
/// Серия правок (перетаскивание, добавление в плейлисты) даёт одну запись.
const LIBRARY_SAVE_DELAY: Duration = Duration::from_millis(400);
/// Пауза перед повтором, если запись не удалась.
const LIBRARY_RETRY_DELAY: Duration = Duration::from_secs(5);
/// Сколько сообщений плагинов разбираем за один вызов `pump`.
const MAX_PLUGIN_MESSAGES_PER_PUMP: usize = 256;
/// Текст ошибки `add_folder`, который плагинам не нужно считать ошибкой.
const FOLDER_EXISTS: &str = "Folder already added";

/// Что плагины уже знают о плеере: по отличиям понимаем, какие события слать.
#[derive(Default)]
struct PlugSnap {
    track: Option<String>,
    index: usize,
    playing: bool,
    repeat: u8,
    shuffle: bool,
    volume: i32,
    queue_gen: u64,
}

pub struct Core {
    pub state: LibraryState,
    pub player: Option<Player>,
    pub settings: Settings,
    pub host: Host,
    pub status: String,
    /// Идёт сканирование папок.
    pub busy: bool,
    incoming: Receiver<Incoming>,
    scan_requested: bool,
    /// Пока шёл скан, просили ещё один (например, добавили папку): повторим после него.
    scan_again: bool,
    /// Когда записать библиотеку на диск; `None` — всё уже записано.
    library_save_due: Option<Instant>,
    settings_dirty: bool,
    settings_saved_at: Instant,
    plug_snap: PlugSnap,
    /// Значение `Host::ready_gen`, для которого начальное состояние уже разослано.
    plug_ready_gen: u64,
    /// Когда и с какой позицией последний раз отправили плагинам `state`.
    plug_sent: Option<(Instant, f32, bool)>,
    /// Плагины, настройки которых изменились и ждут отправки `config_changed`.
    plug_cfg_dirty: HashSet<String>,
}

/// Результат подготовки импорта (чтение файла и тегов идёт в фоне).
#[derive(Debug, Clone)]
pub struct ImportOutcome {
    pub name: String,
    pub ids: Vec<String>,
    /// Треки, которых ещё не было в библиотеке.
    pub new_tracks: Vec<Track>,
}

pub fn default_export_path(name: &str, m3u: bool) -> String {
    model::exports_dir()
        .join(format!(
            "{}.{}",
            transfer::safe_stem(name),
            if m3u { "m3u" } else { "zip" }
        ))
        .to_string_lossy()
        .to_string()
}

/// Экспорт плейлиста в `.m3u` или `.zip`. Ничего не знает про `Core`, поэтому годится для фона.
pub fn run_export(
    path_text: &str,
    playlist: Playlist,
    tracks: Vec<Track>,
    zip: bool,
    include_audio: bool,
) -> Result<String, String> {
    let mut path = model::expand_tilde(path_text);
    if path.as_os_str().is_empty() {
        path = std::path::PathBuf::from(default_export_path(&playlist.name, !zip));
    }
    // Нужное расширение оставляем; расширение «чужого» экспорта (в поле осталось
    // `list.m3u`, а нажали EXPORT .ZIP) подменяем; любой другой хвост — часть имени
    // («Vol. 2»), поэтому расширение дописывается, а не заменяет его.
    let wanted: &[&str] = if zip { &["zip"] } else { &["m3u", "m3u8"] };
    let current = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match current.as_deref() {
        Some(ext) if wanted.contains(&ext) => {}
        Some("m3u" | "m3u8" | "zip") => {
            path.set_extension(wanted[0]);
        }
        _ => {
            let mut text = path.into_os_string();
            text.push(".");
            text.push(wanted[0]);
            path = std::path::PathBuf::from(text);
        }
    }
    let result = if zip {
        transfer::export_zip(&path, &playlist, &tracks, include_audio)
    } else {
        transfer::export_m3u(&path, &tracks)
    };
    match result {
        Ok(()) => Ok(path.to_string_lossy().to_string()),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// Читает `.m3u`/`.m3u8`/`.zip` и сопоставляет файлы с библиотекой (в фоне).
pub fn prepare_import(path_text: &str, known: &[Track]) -> Result<ImportOutcome, String> {
    let path = model::expand_tilde(path_text);
    if !path.is_file() {
        return Err(format!("File not found: {}", path.display()));
    }
    let is_zip = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("zip"))
        .unwrap_or(false);
    let (name, files) = if is_zip {
        transfer::import_zip(&path).map_err(|e| format!("Import failed: {e:#}"))?
    } else {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Import".into());
        let files = transfer::read_m3u(&path).map_err(|e| format!("Import failed: {e:#}"))?;
        (name, files)
    };

    // Индексы строятся один раз: раньше каждый файл плейлиста искался линейным
    // проходом по всей библиотеке (O(файлы × треки)).
    let by_path: HashMap<&str, &str> = known
        .iter()
        .map(|t| (t.path.as_str(), t.id.as_str()))
        .collect();
    let mut by_name: HashMap<String, &str> = HashMap::new();
    for t in known {
        by_name.entry(t.file_name()).or_insert(t.id.as_str());
    }
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut new_tracks: Vec<Track> = Vec::new();
    let mut new_by_path: HashMap<String, String> = HashMap::new();
    for file in files {
        let exists = file.is_file();
        // Картинка или видео из плейлиста — не трек: плеер их не воспроизведёт.
        if exists && !crate::library::is_audio(&file) {
            continue;
        }
        let id: Option<String> = if !exists {
            // Файла нет по записанному пути — пробуем найти по имени в библиотеке.
            let wanted = file
                .file_name()
                .map(|f| f.to_string_lossy().to_string())
                .unwrap_or_default();
            by_name.get(wanted.as_str()).map(|id| id.to_string())
        } else {
            let path_str = file.to_string_lossy().to_string();
            if let Some(id) = by_path.get(path_str.as_str()) {
                Some(id.to_string())
            } else if let Some(id) = new_by_path.get(&path_str) {
                Some(id.clone())
            } else {
                let track = crate::library::make_track(&file);
                let id = track.id.clone();
                new_by_path.insert(path_str, id.clone());
                new_tracks.push(track);
                Some(id)
            }
        };
        if let Some(id) = id {
            if seen.insert(id.clone()) {
                ids.push(id);
            }
        }
    }
    if ids.is_empty() {
        return Err("No playable tracks in the playlist".into());
    }
    Ok(ImportOutcome {
        name,
        ids,
        new_tracks,
    })
}

/// Записывает название, исполнителя и альбом в теги файла (в фоне) и
/// возвращает обновлённый трек с прежним id. Пустое поле удаляет тег.
/// Битый файл не должен ронять плеер: паника библиотеки тегов превращается в ошибку.
pub fn write_tags(
    track: Track,
    title: String,
    artist: String,
    album: String,
    cover_path: Option<String>,
) -> Result<Track, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        write_tags_inner(track, title, artist, album, cover_path)
    }))
    .unwrap_or_else(|_| Err("The tag library failed on this file".to_string()))
}

fn write_tags_inner(
    track: Track,
    title: String,
    artist: String,
    album: String,
    cover_path: Option<String>,
) -> Result<Track, String> {
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::*;
    use lofty::tag::Tag;

    let path = std::path::Path::new(&track.path);
    let mut tagged = lofty::read_from_path(path).map_err(|e| format!("Cannot read tags: {e}"))?;
    if tagged.primary_tag().is_none() {
        let kind = tagged.primary_tag_type();
        tagged.insert_tag(Tag::new(kind));
    }
    let tag = tagged
        .primary_tag_mut()
        .ok_or_else(|| "This format has no tags".to_string())?;
    let (title, artist, album) = (title.trim(), artist.trim(), album.trim());
    if title.is_empty() {
        tag.remove_title();
    } else {
        tag.set_title(title.to_string());
    }
    if artist.is_empty() {
        tag.remove_artist();
    } else {
        tag.set_artist(artist.to_string());
    }
    if album.is_empty() {
        tag.remove_album();
    } else {
        tag.set_album(album.to_string());
    }
    if let Some(cover_path) = cover_path {
        let cover_bytes = std::fs::read(model::expand_tilde(&cover_path))
            .map_err(|e| format!("Cannot read cover image: {e}"))?;
        let format = image::guess_format(&cover_bytes)
            .map_err(|e| format!("Unsupported cover image: {e}"))?;
        let mime = match format {
            image::ImageFormat::Png => MimeType::Png,
            image::ImageFormat::Jpeg => MimeType::Jpeg,
            _ => return Err("Cover must be a PNG or JPEG image".into()),
        };
        image::load_from_memory(&cover_bytes).map_err(|e| format!("Invalid cover image: {e}"))?;
        let picture =
            Picture::new_unchecked(PictureType::CoverFront, Some(mime), None, cover_bytes);
        // Заменяем именно переднюю обложку (её и показывает `cover::load`),
        // а не первую попавшуюся картинку; если передней нет — добавляем.
        let front = tag
            .pictures()
            .iter()
            .position(|p| matches!(p.pic_type(), PictureType::CoverFront));
        match front {
            Some(index) => {
                tag.set_picture(index, picture);
            }
            None => {
                tag.push_picture(picture);
            }
        }
    }
    tag.save_to_path(path, WriteOptions::default())
        .map_err(|e| format!("Cannot write tags: {e}"))?;
    let mut updated = crate::library::make_track(path);
    updated.id = track.id;
    Ok(updated)
}

fn repeat_name(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::One => "one",
    }
}

fn invalid(msg: &str) -> (&'static str, String) {
    ("invalid_params", msg.to_string())
}

fn string_list(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

impl Core {
    pub fn new() -> Self {
        let settings = Settings::load();
        let mut player = Player::new().ok();
        if let Some(p) = &mut player {
            p.set_volume(settings.volume);
        }
        let (tx, incoming) = mpsc::channel();
        let host = Host::scan(std::sync::Arc::new(move |message: Incoming| {
            let _ = tx.send(message);
        }));
        let mut core = Core {
            state: LibraryState::load(),
            player,
            settings,
            host,
            status: "Ready".into(),
            busy: false,
            incoming,
            scan_requested: false,
            scan_again: false,
            library_save_due: None,
            settings_dirty: false,
            settings_saved_at: Instant::now(),
            plug_snap: PlugSnap::default(),
            plug_ready_gen: 0,
            plug_sent: None,
            plug_cfg_dirty: HashSet::new(),
        };
        core.autostart_plugins();
        core
    }

    // ───────────── библиотека ─────────────

    /// Нормализует библиотеку, сохраняет её и уведомляет плагины.
    pub fn persist(&mut self) {
        self.state.normalize();
        self.save_library();
    }

    /// Сохранение без нормализации — когда менялись только плейлисты или папки,
    /// а набор треков остался прежним. Запись на диск откладывается на долю секунды
    /// (`pump`): серия правок даёт одну запись, а не по нескольку мегабайт на каждую.
    fn save_library(&mut self) {
        if self.library_save_due.is_none() {
            self.library_save_due = Some(Instant::now() + LIBRARY_SAVE_DELAY);
        }
        self.plugin_library_changed();
    }

    /// Записывает библиотеку на диск, если есть несохранённые изменения.
    fn flush_library(&mut self) {
        if self.library_save_due.take().is_none() {
            return;
        }
        if let Err(e) = self.state.save() {
            self.status = format!("Cannot save the library: {e}");
            // Диск мог быть временно занят — повторим чуть позже.
            self.library_save_due = Some(Instant::now() + LIBRARY_RETRY_DELAY);
        }
    }

    /// Есть несохранённые изменения: интерфейс в этом случае вызывает `pump` чаще.
    pub fn has_pending_saves(&self) -> bool {
        self.settings_dirty || self.library_save_due.is_some()
    }

    /// Просит интерфейс запустить сканирование (интерфейс сам решает, как: задачей, потоком).
    /// Если скан уже идёт, он не увидит новых папок и правок — просьба запоминается
    /// и выполняется сразу после него.
    pub fn request_scan(&mut self) {
        if self.busy {
            self.scan_again = true;
        } else {
            self.scan_requested = true;
        }
    }

    pub fn take_scan_request(&mut self) -> bool {
        std::mem::take(&mut self.scan_requested)
    }

    /// Результат сканирования: треки из плейлистов, лежащие вне папок, не теряем.
    pub fn apply_scan(&mut self, mut tracks: Vec<Track>) {
        let extra: Vec<Track> = {
            let found: HashSet<&str> = tracks.iter().map(|t| t.path.as_str()).collect();
            let used: HashSet<&str> = self
                .state
                .playlists
                .iter()
                .flat_map(|p| p.track_ids.iter())
                .map(String::as_str)
                .collect();
            self.state
                .tracks
                .iter()
                .filter(|old| {
                    used.contains(old.id.as_str())
                        && !found.contains(old.path.as_str())
                        && std::path::Path::new(&old.path).is_file()
                })
                .cloned()
                .collect()
        };
        tracks.extend(extra);
        if self.state.manual_order {
            // Порядок, выбранный пользователем, важнее сортировки скана;
            // новые файлы (их позиции нет) остаются в конце.
            let pos: HashMap<&str, usize> = self
                .state
                .tracks
                .iter()
                .enumerate()
                .map(|(i, t)| (t.path.as_str(), i))
                .collect();
            tracks.sort_by_key(|t| pos.get(t.path.as_str()).copied().unwrap_or(usize::MAX));
        }
        self.status = format!("Library: {} tracks", tracks.len());
        self.state.tracks = tracks;
        self.busy = false;
        if std::mem::take(&mut self.scan_again) {
            self.scan_requested = true;
        }
        self.persist();
    }

    /// Переносит трек библиотеки с позиции `from` на `to` (перетаскивание).
    pub fn move_track_to(&mut self, from: usize, to: usize) -> bool {
        let n = self.state.tracks.len();
        if from >= n || to >= n || from == to {
            return false;
        }
        let track = self.state.tracks.remove(from);
        self.state.tracks.insert(to, track);
        self.state.manual_order = true;
        self.save_library();
        true
    }

    /// Переносит трек внутри плейлиста с позиции `from` на `to`.
    pub fn move_in_playlist_to(&mut self, playlist_id: &str, from: usize, to: usize) -> bool {
        let Some(playlist) = self.state.playlist_mut(playlist_id) else {
            return false;
        };
        let n = playlist.track_ids.len();
        if from >= n || to >= n || from == to {
            return false;
        }
        let id = playlist.track_ids.remove(from);
        playlist.track_ids.insert(to, id);
        self.save_library();
        true
    }

    /// Убирает трек из библиотеки и плейлистов. Файл на диске остаётся,
    /// но скан его больше не добавит (см. `restore_removed`).
    pub fn remove_track(&mut self, id: &str) {
        let Some(path) = self.state.track(id).map(|t| t.path.clone()) else {
            return;
        };
        self.state.tracks.retain(|t| t.id != id);
        if !self.state.excluded.contains(&path) {
            self.state.excluded.push(path);
        }
        self.persist();
    }

    /// Возвращает в библиотеку всё, что было убрано, и запускает скан.
    pub fn restore_removed(&mut self) {
        if self.state.excluded.is_empty() {
            return;
        }
        self.state.excluded.clear();
        self.save_library();
        self.request_scan();
    }

    /// Подменяет трек (после правки тегов); id сохраняется, чтобы не потерять плейлисты.
    pub fn replace_track(&mut self, track: Track) {
        // Очередь плеера хранит копии треков: без этого в панели остались бы старые теги.
        if let Some(player) = &mut self.player {
            for queued in player.queue.iter_mut().filter(|t| t.id == track.id) {
                *queued = track.clone();
            }
        }
        if let Some(slot) = self.state.tracks.iter_mut().find(|t| t.id == track.id) {
            *slot = track;
            self.save_library();
        }
    }

    pub fn add_folder(&mut self, path: &str) -> Result<(), String> {
        // `~/Music`, слэш на конце и повторные разделители приводим к одному виду,
        // иначе одна и та же папка добавлялась бы дважды.
        let path = model::expand_tilde(path)
            .components()
            .collect::<std::path::PathBuf>()
            .to_string_lossy()
            .into_owned();
        if !std::path::Path::new(&path).is_dir() {
            return Err("Folder not found".into());
        }
        if self.state.folders.contains(&path) {
            return Err(FOLDER_EXISTS.into());
        }
        self.state.folders.push(path);
        self.save_library();
        self.request_scan();
        Ok(())
    }

    pub fn remove_folder(&mut self, path: &str) {
        self.state.folders.retain(|f| f != path);
        self.save_library();
        self.request_scan();
    }

    pub fn create_playlist(&mut self, name: &str) -> String {
        let id = short_id(&format!(
            "{name}:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        self.state.playlists.push(Playlist {
            id: id.clone(),
            name: name.to_string(),
            track_ids: Vec::new(),
        });
        self.save_library();
        id
    }

    pub fn rename_playlist(&mut self, id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if let Some(playlist) = self.state.playlist_mut(id) {
            playlist.name = name.to_string();
            self.save_library();
        }
    }

    pub fn delete_playlist(&mut self, id: &str) {
        self.state.playlists.retain(|p| p.id != id);
        self.save_library();
    }

    pub fn remove_from_playlist(&mut self, playlist_id: &str, track_id: &str) {
        if let Some(playlist) = self.state.playlist_mut(playlist_id) {
            playlist.track_ids.retain(|t| t != track_id);
            self.save_library();
        }
    }

    /// Добавляет один трек в плейлист; false, если он там уже есть.
    pub fn add_to_playlist(&mut self, playlist_id: &str, track_id: &str) -> bool {
        let Some(playlist) = self.state.playlist_mut(playlist_id) else {
            return false;
        };
        if playlist.track_ids.iter().any(|t| t == track_id) {
            return false;
        }
        playlist.track_ids.push(track_id.to_string());
        self.save_library();
        true
    }

    /// Создаёт плейлист из результата импорта. Возвращает его id и строку для статуса.
    pub fn apply_import(&mut self, outcome: ImportOutcome) -> (String, String) {
        let ImportOutcome {
            name,
            ids,
            new_tracks,
        } = outcome;
        let mut added = 0usize;
        if !new_tracks.is_empty() {
            // Путь мог появиться в библиотеке, пока шла подготовка импорта в фоне.
            let mut known: HashSet<String> =
                self.state.tracks.iter().map(|t| t.path.clone()).collect();
            for track in new_tracks {
                if known.insert(track.path.clone()) {
                    self.state.tracks.push(track);
                    added += 1;
                }
            }
        }
        // id с отметкой времени: повторный импорт того же файла не даёт двух плейлистов с одним id.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut playlist = Playlist {
            id: short_id(&format!("{name}:{}:{stamp}", ids.len())),
            name: name.clone(),
            track_ids: ids,
        };
        let mut n = 2;
        while self.state.playlists.iter().any(|p| p.name == playlist.name) {
            playlist.name = format!("{name} ({n})");
            n += 1;
        }
        let status = format!(
            "Imported: {} tracks ({added} new in the library)",
            playlist.track_ids.len()
        );
        let id = playlist.id.clone();
        self.state.playlists.push(playlist);
        self.persist();
        (id, status)
    }

    /// Добавляет в плейлист существующие треки, которых там ещё нет.
    fn extend_playlist(&mut self, playlist_id: &str, track_ids: &[String]) {
        let wanted: Vec<String> = {
            let known: HashSet<&str> = self.state.tracks.iter().map(|t| t.id.as_str()).collect();
            track_ids
                .iter()
                .filter(|id| known.contains(id.as_str()))
                .cloned()
                .collect()
        };
        let mut changed = false;
        if let Some(playlist) = self.state.playlist_mut(playlist_id) {
            let mut have: HashSet<String> = playlist.track_ids.iter().cloned().collect();
            for id in wanted {
                if have.insert(id.clone()) {
                    playlist.track_ids.push(id);
                    changed = true;
                }
            }
        }
        if changed {
            self.save_library();
        }
    }

    // ───────────── настройки ─────────────

    pub fn mark_settings_dirty(&mut self) {
        self.settings_dirty = true;
        self.settings_saved_at = Instant::now();
    }

    pub fn save_settings(&mut self) {
        if let Some(player) = &self.player {
            self.settings.volume = player.volume;
        }
        if let Err(e) = self.settings.save() {
            self.status = format!("Cannot save settings: {e}");
        }
        self.settings_dirty = false;
        self.settings_saved_at = Instant::now();
        self.plugin_config_flush();
    }

    // ───────────── плагины ─────────────

    /// Запускает включённые плагины. Если права плагина выросли с момента подтверждения,
    /// он остаётся выключенным, пока пользователь не включит его снова.
    pub fn autostart_plugins(&mut self) {
        for idx in 0..self.host.plugins.len() {
            let manifest = &self.host.plugins[idx].manifest;
            let id = manifest.id.clone();
            let perms = manifest.permissions.clone();
            let name = manifest.name.clone();
            let Some(entry) = self.settings.plugins.get_mut(&id) else {
                continue;
            };
            if !entry.enabled {
                continue;
            }
            if perms.iter().any(|p| !entry.granted.contains(p)) {
                entry.enabled = false;
                self.settings_dirty = true;
                self.status = format!("[{name}] permissions changed — re-enable the plugin");
                continue;
            }
            let config = entry.config.clone();
            self.host.start(idx, &config);
        }
    }

    pub fn rescan_plugins(&mut self) {
        self.host.rescan();
        self.autostart_plugins();
    }

    pub fn plugin_enabled(&self, id: &str) -> bool {
        self.settings
            .plugins
            .get(id)
            .map(|e| e.enabled)
            .unwrap_or(false)
    }

    pub fn set_plugin_enabled(&mut self, idx: usize, on: bool) {
        let Some(plugin) = self.host.plugins.get(idx) else {
            return;
        };
        let id = plugin.manifest.id.clone();
        let perms = plugin.manifest.permissions.clone();
        let entry = self.settings.plugins.entry(id).or_default();
        entry.enabled = on;
        if on {
            entry.granted = perms;
            let config = entry.config.clone();
            self.host.start(idx, &config);
        } else {
            self.host.stop(idx);
        }
        self.mark_settings_dirty();
    }

    pub fn set_plugin_config(&mut self, id: &str, key: &str, value: Value) {
        let entry = self.settings.plugins.entry(id.to_string()).or_default();
        entry.config.insert(key.to_string(), value);
        self.plug_cfg_dirty.insert(id.to_string());
        self.mark_settings_dirty();
    }

    pub fn plugin_command(&mut self, idx: usize, name: &str) {
        if !self.host.send_command(idx, name) {
            self.status = "Plugin is not ready for commands".into();
        }
    }

    /// Отправляет `config_changed` плагинам, чьи настройки менялись (при сохранении).
    fn plugin_config_flush(&mut self) {
        let ids: Vec<String> = self.plug_cfg_dirty.drain().collect();
        for id in ids {
            let Some(idx) = self.host.position(&id) else {
                continue;
            };
            let config = self
                .settings
                .plugins
                .get(&id)
                .map(|e| e.config.clone())
                .unwrap_or_default();
            self.host.update_config(idx, &config);
        }
    }

    /// Вызывать регулярно: забирает сообщения плагинов, обслуживает перезапуски
    /// и таймауты, шлёт события, сохраняет настройки.
    pub fn pump(&mut self) {
        // Не больше пачки за вызов: болтливый плагин не должен занимать интерфейс надолго,
        // остальное разберём на следующем тике.
        for _ in 0..MAX_PLUGIN_MESSAGES_PER_PUMP {
            let Ok(message) = self.incoming.try_recv() else {
                break;
            };
            match self.host.handle(message) {
                Handled::Nothing => {}
                Handled::Status(line) => self.status = line,
                Handled::Request(request) => self.plugin_request(request),
            }
        }
        for note in self.host.tick() {
            self.status = note;
        }
        self.plugin_events();
        if self.settings_dirty && self.settings_saved_at.elapsed() > Duration::from_millis(700) {
            self.save_settings();
        }
        if self
            .library_save_due
            .is_some_and(|due| Instant::now() >= due)
        {
            self.flush_library();
        }
    }

    fn plugin_request(&mut self, request: Request) {
        let Request {
            plugin,
            id,
            method,
            params,
        } = request;
        let result = match required_permission(&method) {
            None => Err(("unknown_method", format!("unknown method '{method}'"))),
            Some(perm) if !self.host.has_perm(plugin, perm) => {
                Err(("permission_denied", format!("permission {perm} required")))
            }
            Some(_) => self.plugin_call(&method, &params),
        };
        self.host.respond(plugin, id, result);
    }

    fn player_state(&self) -> Value {
        match &self.player {
            Some(p) => json!({
                "playing": p.playing,
                "position_ms": (p.position.max(0.0) * 1000.0) as u64,
                "duration_ms": (p.duration.max(0.0) * 1000.0) as u64,
                "volume": p.volume,
                "repeat": repeat_name(p.repeat),
                "shuffle": p.shuffle,
                "queue_index": p.index,
                "queue_len": p.queue.len(),
            }),
            None => json!({
                "playing": false, "position_ms": 0, "duration_ms": 0, "volume": 0.0,
                "repeat": "off", "shuffle": false, "queue_index": 0, "queue_len": 0,
            }),
        }
    }

    fn plugin_call(&mut self, method: &str, params: &Value) -> ApiResult {
        match method {
            "player.get_state" => Ok(self.player_state()),
            "player.get_queue" => Ok(match &self.player {
                Some(p) => json!({
                    "track_ids": p.queue.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(),
                    "index": p.index,
                }),
                None => json!({"track_ids": [], "index": 0}),
            }),

            "player.play_queue" => {
                let ids = string_list(params, "track_ids");
                let start = params.get("start").and_then(Value::as_u64).unwrap_or(0) as usize;
                // Индекс `start` относится к присланному списку, а неизвестные id из него
                // выпадают — считаем, сколько известных треков стоит перед `start`.
                // Индекс id → трек строится один раз (раньше был поиск по всей библиотеке
                // на каждый id: на очереди в тысячи треков это заметная пауза в интерфейсе).
                let (tracks, first) = {
                    let by_id: HashMap<&str, &Track> = self
                        .state
                        .tracks
                        .iter()
                        .map(|t| (t.id.as_str(), t))
                        .collect();
                    let mut tracks: Vec<Track> = Vec::with_capacity(ids.len());
                    let mut first = 0usize;
                    for (i, id) in ids.iter().enumerate() {
                        if let Some(track) = by_id.get(id.as_str()) {
                            if i < start {
                                first += 1;
                            }
                            tracks.push((*track).clone());
                        }
                    }
                    (tracks, first)
                };
                if tracks.is_empty() {
                    return Err(("not_found", "no known track_id".into()));
                }
                match self.player.as_mut() {
                    Some(player) => player.play_queue(tracks, first),
                    None => return Err(("internal", "audio output unavailable".into())),
                }
                Ok(json!({}))
            }
            "player.play" | "player.pause" | "player.toggle" | "player.next" | "player.prev"
            | "player.seek" | "player.set_volume" => {
                // Параметры проверяем до обращения к плееру.
                let seek_to = match method {
                    "player.seek" => Some(
                        params
                            .get("position_ms")
                            .and_then(Value::as_f64)
                            .filter(|v| *v >= 0.0)
                            .ok_or_else(|| invalid("position_ms >= 0 required"))?,
                    ),
                    _ => None,
                };
                let volume = match method {
                    "player.set_volume" => Some(
                        params
                            .get("volume")
                            .and_then(Value::as_f64)
                            .filter(|v| (0.0..=1.0).contains(v))
                            .ok_or_else(|| invalid("volume 0.0..1.0 required"))?,
                    ),
                    _ => None,
                };
                let Some(player) = self.player.as_mut() else {
                    return Err(("internal", "audio output unavailable".into()));
                };
                let mut volume_changed = false;
                match method {
                    "player.play" => {
                        if !player.playing {
                            player.toggle();
                        }
                    }
                    "player.pause" => {
                        if player.playing {
                            player.toggle();
                        }
                    }
                    "player.toggle" => player.toggle(),
                    "player.next" => player.next(),
                    "player.prev" => player.prev(),
                    "player.seek" => player.seek(seek_to.unwrap_or(0.0) as f32 / 1000.0),
                    _ => {
                        player.set_volume(volume.unwrap_or(0.0) as f32);
                        volume_changed = true;
                    }
                }
                if volume_changed {
                    self.mark_settings_dirty();
                }
                Ok(json!({}))
            }

            "library.get_tracks" => {
                let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                let limit = params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(200)
                    .min(500) as usize;
                let tracks: Vec<&Track> =
                    self.state.tracks.iter().skip(offset).take(limit).collect();
                Ok(json!({"total": self.state.tracks.len(), "tracks": tracks}))
            }
            "library.get_track" => {
                let id = params
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("field id required"))?;
                match self.state.track(id) {
                    Some(track) => Ok(json!(track)),
                    None => Err(("not_found", "track not found".into())),
                }
            }
            "library.get_playlists" => Ok(json!({"playlists": self.state.playlists})),
            "library.create_playlist" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|n| !n.is_empty() && n.chars().count() <= 200)
                    .ok_or_else(|| invalid("non-empty name up to 200 chars required"))?
                    .to_string();
                let track_ids = string_list(params, "track_ids");
                let id = self.create_playlist(&name);
                self.extend_playlist(&id, &track_ids);
                Ok(json!({"id": id}))
            }
            "library.add_to_playlist" => {
                let id = params
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("field id required"))?
                    .to_string();
                if self.state.playlist(&id).is_none() {
                    return Err(("not_found", "playlist not found".into()));
                }
                let track_ids = string_list(params, "track_ids");
                self.extend_playlist(&id, &track_ids);
                Ok(json!({}))
            }
            "library.get_folders" => Ok(json!({"folders": self.state.folders})),
            "library.add_folder" => {
                let path = params
                    .get("path")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid("field path required"))?;
                match self.add_folder(path) {
                    Ok(()) => Ok(json!({})),
                    // Папка уже в библиотеке — для плагина это успех (операция идемпотентна).
                    Err(reason) if reason == FOLDER_EXISTS => Ok(json!({})),
                    Err(reason) => Err(("not_found", reason)),
                }
            }
            "settings.get_theme" => {
                let (name, colors) = crate::theme::export();
                let colors: serde_json::Map<String, Value> = colors
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), json!(v)))
                    .collect();
                Ok(json!({"name": name, "colors": colors}))
            }
            "settings.set_theme" => {
                let colors: Vec<(String, String)> = params
                    .get("colors")
                    .and_then(Value::as_object)
                    .ok_or_else(|| invalid("object colors required"))?
                    .iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect();
                let name = params.get("name").and_then(Value::as_str);
                crate::theme::apply(name, &colors).map_err(|e| ("invalid_params", e))?;
                Ok(json!({}))
            }
            "library.rescan" => {
                // Если скан уже идёт, `request_scan` запомнит просьбу и выполнит её сразу
                // после него: файлы, появившиеся за это время, не должны ждать следующего раза.
                self.request_scan();
                Ok(json!({}))
            }
            _ => Err(("unknown_method", format!("unknown method '{method}'"))),
        }
    }

    /// События плеера плагинам. Пока ни один плагин не готов, ничего не считается.
    fn plugin_events(&mut self) {
        if !self.host.any_ready() {
            return;
        }
        if self.plug_ready_gen != self.host.ready_gen {
            // Плагин стал готов уже во время игры: события о треке и очереди приходят
            // только при изменении, поэтому новичку нужно разослать всё заново.
            // Для остальных это повтор, а события — просто уведомления.
            self.plug_ready_gen = self.host.ready_gen;
            self.plug_snap = PlugSnap::default();
            self.plug_sent = None;
        }
        let Some(p) = &self.player else {
            return;
        };
        let snap = PlugSnap {
            track: p.current().map(|t| t.id.clone()),
            index: p.index,
            playing: p.playing,
            repeat: p.repeat as u8,
            shuffle: p.shuffle,
            volume: (p.volume * 100.0).round() as i32,
            queue_gen: p.queue_gen,
        };
        let position = p.position;
        let old = &self.plug_snap;
        let queue_changed = snap.queue_gen != old.queue_gen;
        let track_changed = queue_changed || snap.track != old.track || snap.index != old.index;
        let mut state_due = track_changed
            || snap.playing != old.playing
            || snap.repeat != old.repeat
            || snap.shuffle != old.shuffle
            || snap.volume != old.volume;
        match self.plug_sent {
            None => state_due = true,
            Some((at, pos, was_playing)) => {
                let elapsed = at.elapsed().as_secs_f32();
                if snap.playing {
                    let expected = if was_playing { pos + elapsed } else { pos };
                    // Раз в секунду во время игры и сразу при перемотке.
                    if elapsed >= 1.0 || (position - expected).abs() > 1.5 {
                        state_due = true;
                    }
                } else if (position - pos).abs() > 0.5 {
                    state_due = true;
                }
            }
        }

        if queue_changed {
            let ids: Vec<&str> = self
                .player
                .as_ref()
                .map(|p| p.queue.iter().map(|t| t.id.as_str()).collect())
                .unwrap_or_default();
            self.host.broadcast(
                "queue_changed",
                json!({"track_ids": ids, "index": snap.index}),
            );
        }
        if track_changed {
            let track = self
                .player
                .as_ref()
                .and_then(|p| p.current())
                .map(|t| json!(t))
                .unwrap_or(Value::Null);
            self.host.broadcast(
                "track_changed",
                json!({"track": track, "index": snap.index}),
            );
        }
        if state_due {
            self.host.broadcast("state", self.player_state());
            self.plug_sent = Some((Instant::now(), position, snap.playing));
        }
        self.plug_snap = snap;
    }

    fn plugin_library_changed(&self) {
        if self.host.any_ready() {
            self.host.broadcast(
                "library_changed",
                json!({
                    "tracks": self.state.tracks.len(),
                    "playlists": self.state.playlists.len(),
                }),
            );
        }
    }
}

impl Drop for Core {
    /// Отложенные записи (библиотека, настройки) не должны теряться при закрытии окна.
    /// Плагины после этого останавливает `Drop` хоста.
    fn drop(&mut self) {
        self.flush_library();
        if self.settings_dirty {
            self.save_settings();
        }
    }
}
