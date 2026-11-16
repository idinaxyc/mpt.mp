use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const AUDIO_EXTS: [&str; 6] = ["mp3", "flac", "ogg", "oga", "wav", "m4a"];

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub path: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration_ms: u64,
    pub size: u64,
    /// Время изменения файла (секунды Unix): по нему скан понимает, что теги перечитывать не нужно.
    #[serde(default)]
    pub mtime: u64,
}

impl Track {
    pub fn file_name(&self) -> String {
        Path::new(&self.path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    pub fn duration_text(&self) -> String {
        fmt_secs(self.duration_ms / 1000)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub track_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LibraryState {
    pub tracks: Vec<Track>,
    pub playlists: Vec<Playlist>,
    pub folders: Vec<String>,
    /// Пользователь менял порядок треков в библиотеке: скан сохраняет его,
    /// а новые файлы добавляет в конец.
    #[serde(default)]
    pub manual_order: bool,
    /// Пути треков, убранных из библиотеки (файлы на диске не трогаются):
    /// скан их пропускает.
    #[serde(default)]
    pub excluded: Vec<String>,
}

impl LibraryState {
    pub fn track(&self, id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn playlist(&self, id: &str) -> Option<&Playlist> {
        self.playlists.iter().find(|p| p.id == id)
    }

    pub fn playlist_mut(&mut self, id: &str) -> Option<&mut Playlist> {
        self.playlists.iter_mut().find(|p| p.id == id)
    }

    /// Треки по списку id в том же порядке; неизвестные id пропускаются. Индекс
    /// id → трек строится один раз, а не поиском по всей библиотеке для каждого id.
    pub fn tracks_by_ids(&self, ids: &[String]) -> Vec<Track> {
        let by_id: HashMap<&str, &Track> = self.tracks.iter().map(|t| (t.id.as_str(), t)).collect();
        ids.iter()
            .filter_map(|id| by_id.get(id.as_str()).map(|t| (*t).clone()))
            .collect()
    }

    /// Треки плейлиста в его порядке.
    pub fn tracks_of(&self, playlist: &Playlist) -> Vec<Track> {
        self.tracks_by_ids(&playlist.track_ids)
    }

    /// Убирает ссылки на несуществующие треки, дубликаты путей.
    /// Без копирования строк: множества хранят `&str`.
    pub fn normalize(&mut self) {
        let dup: Vec<bool> = {
            let mut seen = std::collections::HashSet::with_capacity(self.tracks.len());
            self.tracks
                .iter()
                .map(|t| !seen.insert(t.path.as_str()))
                .collect()
        };
        if dup.iter().any(|d| *d) {
            let mut i = 0;
            self.tracks.retain(|_| {
                let keep = !dup[i];
                i += 1;
                keep
            });
        }
        let ids: std::collections::HashSet<&str> =
            self.tracks.iter().map(|t| t.id.as_str()).collect();
        for p in &mut self.playlists {
            p.track_ids.retain(|id| ids.contains(id.as_str()));
        }
    }

    /// Компактный JSON: на большой библиотеке заметно быстрее и меньше, чем `pretty`.
    pub fn save(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let json = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        write_atomic(&data_dir().join("library.json"), &json)
    }

    pub fn load() -> Self {
        let file = data_dir().join("library.json");
        let text = std::fs::read_to_string(&file).ok();
        let parsed = text
            .as_deref()
            .and_then(|s| serde_json::from_str::<LibraryState>(s).ok());
        if parsed.is_none() && text.is_some() {
            // Файл есть, но не читается: сохраняем копию, иначе следующий save()
            // затрёт плейлисты пустым состоянием.
            let _ = std::fs::copy(&file, file.with_extension("json.bad"));
        }
        let mut state = parsed.unwrap_or_default();
        state.normalize();
        state
    }
}

/// Запись через временный файл + rename: обрыв посреди записи не оставит
/// наполовину записанный JSON. Данные сбрасываются на диск до переименования,
/// иначе после сбоя питания на месте файла может оказаться пустой.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    // Счётчик в имени: два одновременных сохранения одного файла не делят временный.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp-{}-{n}", std::process::id()));

    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        // Windows не переименовывает открытый файл.
        drop(file);
        replace_file(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // MoveFileExW заменяет существующий файл назначения, а запись сквозная.
    let ok = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// `~`, `~/…` и `~\…` → домашняя папка; остальной текст без изменений
/// (в полях ввода пути обычно набирают именно так).
pub fn expand_tilde(text: &str) -> PathBuf {
    let text = text.trim();
    if let Some(home) = dirs::home_dir() {
        if text == "~" {
            return home;
        }
        if let Some(rest) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
            return home.join(rest);
        }
    }
    PathBuf::from(text)
}

pub fn root_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mpt.mp")
}

pub fn data_dir() -> PathBuf {
    root_dir().join("data")
}

pub fn imports_dir() -> PathBuf {
    root_dir().join("imports")
}

pub fn exports_dir() -> PathBuf {
    root_dir().join("exports")
}

/// Папка с плагинами: `~/mpt.mp/plugins/<id>/plugin.json`.
pub fn plugins_dir() -> PathBuf {
    root_dir().join("plugins")
}

/// Единственное место, куда плагину разрешено писать свои данные (`MPT_DATA_DIR`).
pub fn plugin_data_dir(id: &str) -> PathBuf {
    root_dir().join("plugin-data").join(id)
}

pub fn ensure_dirs() {
    for d in [data_dir(), imports_dir(), exports_dir(), plugins_dir()] {
        let _ = std::fs::create_dir_all(d);
    }
}

pub fn fmt_secs(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn short_id(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    hex::encode(&hasher.finalize()[..8])
}
