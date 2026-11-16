//! Интерфейс на iced 0.13.
//!
//! Страницы: библиотека (плейлисты), папки с музыкой, обмен (.m3u/.zip), плагины. Поиск — в
//! верхней панели рядом со вкладками: сначала треки, затем подходящие плагины
//! (Ctrl+F или «/» — фокус). Вся логика — в `engine.rs`; здесь только отображение
//! и преобразование сообщений.
//! Клавиши: Пробел, N/P, ←/→ (±5 с), ↑/↓ (громкость), C (обложка), S, R, Esc.
//! Плейлисты: создание, переименование, удаление, «+»/«−» у треков.
//! Режим правки «[E]»: перетаскивание строк за «=», удаление, правка тегов и обложки.
//!
//! Принцип «лёгкости»: iced перерисовывает окно только по событиям; единственный таймер
//! — `Tick` (250 мс при воспроизведении, работающих плагинах или несохранённых правках,
//! иначе 2 с).

use crate::engine::{self, Core, ImportOutcome};
use crate::model::{fmt_secs, Track};
use crate::player::RepeatMode;
use crate::plugins::{Level, Plugin, RunState};
use crate::theme::{pal, Rgb};
use iced::keyboard::{self, key::Named, Key};
use iced::widget::image::Handle;
use iced::widget::{
    button, column, container, horizontal_space, image, mouse_area, pick_list, row, slider, stack,
    text, text_input, vertical_space,
};
use iced::{
    window, Alignment, Background, Border, Color, ContentFit, Element, Font, Length, Size,
    Subscription, Task, Theme,
};
use serde_json::{json, Value};
use std::time::Duration;

const NEW_PLAYLIST_ID: &str = "new-playlist-input";
const RENAME_ID: &str = "rename-playlist-input";
const SEARCH_ID: &str = "search-input";
const LIST_ID: &str = "track-list";
/// Сколько плагинов показывать под списком треков (остальные — на странице плагинов).
const PLUGIN_HITS_SHOWN: usize = 3;

const ROW_H: f32 = 44.0;
/// Сколько строк рисуем сверх видимых, чтобы прокрутка не мерцала.
const OVERSCAN: usize = 6;
/// Начальная высота окна. Размер окна — верхняя оценка высоты списка: строк рисуется
/// чуть больше, чем нужно, зато после увеличения окна внизу не появляется пустота.
const WINDOW_H: f32 = 820.0;

fn c(color: Rgb) -> Color {
    Color::from_rgb8(color.r, color.g, color.b)
}

/// Пиксельный шрифт для логотипа (assets/fonts/Jersey10-Regular.ttf).
const PIXEL: Font = Font::with_name("Jersey 10");

pub fn run() -> iced::Result {
    iced::application("mpt.mp", App::update, App::view)
        .subscription(App::subscription)
        .theme(App::theme)
        .font(include_bytes!("../../assets/fonts/Jersey10-Regular.ttf").as_slice())
        // Обычный системный шрифт; моноширинный — только для путей и журналов.
        .window(window::Settings {
            size: Size::new(1240.0, WINDOW_H),
            min_size: Some(Size::new(980.0, 620.0)),
            icon: load_icon(),
            ..Default::default()
        })
        .antialiasing(true)
        .run_with(App::new)
}

/// Вертикальная прокрутка без полосы (колесо и тачпад работают как обычно).
fn vscroll<'a>(
    content: impl Into<Element<'a, Message>>,
) -> iced::widget::scrollable::Scrollable<'a, Message> {
    iced::widget::scrollable(content).direction(iced::widget::scrollable::Direction::Vertical(
        iced::widget::scrollable::Scrollbar::new()
            .width(0)
            .scroller_width(0)
            .margin(0),
    ))
}

fn load_icon() -> Option<window::Icon> {
    let img = ::image::load_from_memory(include_bytes!("../../assets/icon-256.png"))
        .ok()?
        .to_rgba8();
    let (width, height) = img.dimensions();
    window::icon::from_rgba(img.into_raw(), width, height).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Library,
    Folders,
    Transfer,
    Plugins,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    Page(Page),
    // библиотека
    Search(String),
    ClearSearch,
    FocusSearch,
    // режим правки
    ToggleEdit,
    /// Нажали «=» у строки `pos`: начало перетаскивания.
    DragStart(usize),
    /// Курсор сдвинулся (координата Y в окне) — только во время перетаскивания.
    DragMove(f32),
    DragEnd,
    RemoveAsk(String),
    RemoveTrack(String),
    TagStart(String),
    TagTitle(String),
    TagArtist(String),
    TagAlbum(String),
    TagCoverPath(String),
    TagSave,
    TagCancel,
    TagSaved(Result<Track, String>),
    RestoreRemoved,
    Select(Option<String>),
    PlayAt(usize),
    Rescan,
    Scanned(Vec<Track>),
    /// Смещение прокрутки списка треков (пиксели).
    Scrolled(f32),
    /// Новая высота окна.
    Resized(f32),
    /// Сообщение-заглушка: им «съедаются» клики и колесо мыши под модальным окном.
    Noop,
    // плеер
    Toggle,
    Next,
    Prev,
    Seeking(f32),
    SeekReleased,
    Volume(f32),
    VolumeReleased,
    CycleRepeat,
    ToggleShuffle,
    SeekBy(f32),
    VolumeBy(f32),
    // обложка и клавиши
    CoverLoaded {
        track_id: String,
        big: Handle,
        small: Handle,
    },
    ToggleCover,
    Escape,
    // плейлисты
    NewPlaylistStart,
    NewPlaylistInput(String),
    NewPlaylistSubmit,
    RenameStart,
    RenameInput(String),
    RenameSubmit,
    DeleteAsk,
    DeleteConfirm,
    AddToPlaylist {
        playlist: String,
        track: String,
    },
    RemoveFromPlaylist(String),
    // импорт и экспорт
    ExportSelect(String),
    ExportPath(String),
    ExportAudio(bool),
    ExportRun(bool),
    Exported(Result<String, String>),
    ImportPath(String),
    ImportRun,
    Imported(Result<ImportOutcome, String>),
    OpenExports,
    // папки
    FolderInput(String),
    AddFolder,
    AddMusicDir,
    RemoveFolder(String),
    // плагины
    PluginToggle(usize, bool),
    PluginConfig {
        id: String,
        key: String,
        value: Value,
    },
    PluginStep {
        id: String,
        key: String,
        delta: i64,
    },
    PluginCommand(usize, String),
    PluginsRescan,
}

/// Строка поиска по треку в нижнем регистре, посчитанная заранее.
/// Заголовок идёт первым: `text[..title_len]` — только название.
struct Hay {
    text: String,
    title_len: usize,
}

/// Перетаскивание строки за «=».
struct Drag {
    from: usize,
    /// Y курсора при первом движении (нажатие не сообщает координат).
    y0: Option<f32>,
    to: usize,
}

/// Как подсветить строку при перетаскивании.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    None,
    Dragged,
    Target,
}

/// Поля формы «править теги».
struct TagEdit {
    track: Track,
    title: String,
    artist: String,
    album: String,
    cover_path: String,
    busy: bool,
}

struct App {
    core: Core,
    /// Режим правки: клик по строке открывает редактор тегов; доступны порядок и удаление.
    edit_mode: bool,
    drag: Option<Drag>,
    tag_edit: Option<TagEdit>,
    /// Трек, для которого ждём второго нажатия «SURE?» при удалении из библиотеки.
    confirm_remove: Option<String>,
    page: Page,
    search: String,
    /// Слова запроса в нижнем регистре (все должны найтись).
    terms: Vec<String>,
    /// Кэш для поиска; индексы совпадают с `core.state.tracks`.
    hay: Vec<Hay>,
    hay_dirty: bool,
    /// Индексы `core.host.plugins`, подходящие под запрос (без запроса — все).
    plugin_hits: Vec<usize>,
    /// После смены списка прокрутку нужно вернуть наверх (виджет хранит её сам).
    scroll_reset: bool,
    /// Выбранный плейлист; `None` — все треки.
    selected: Option<String>,
    /// Индексы `core.state.tracks`, подходящие под поиск и плейлист.
    visible: Vec<usize>,
    /// Смещение прокрутки списка; `window()` по нему выбирает рисуемые строки.
    scroll_y: f32,
    /// Верхняя оценка высоты списка (высота окна).
    view_h: f32,
    /// Позиция ползунка во время перетаскивания.
    seeking: Option<f32>,
    folder_input: String,
    /// Обложка текущего трека: (id трека, большая, маленькая); `None` внутри — ещё грузится.
    cover: Option<(String, Option<(Handle, Handle)>)>,
    cover_open: bool,
    /// Поле «новый плейлист» (None — скрыто).
    new_playlist: Option<String>,
    /// Поле переименования выбранного плейлиста.
    rename: Option<String>,
    confirm_delete: bool,
    export_playlist: Option<String>,
    export_path: String,
    include_audio: bool,
    import_path: String,
    /// Идёт экспорт или импорт в фоне.
    transfer_busy: bool,
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let core = Core::new();
        let mut app = App {
            core,
            edit_mode: false,
            drag: None,
            tag_edit: None,
            confirm_remove: None,
            page: Page::Library,
            search: String::new(),
            terms: Vec::new(),
            hay: Vec::new(),
            hay_dirty: true,
            plugin_hits: Vec::new(),
            scroll_reset: false,
            selected: None,
            visible: Vec::new(),
            scroll_y: 0.0,
            view_h: WINDOW_H,
            seeking: None,
            folder_input: dirs::home_dir()
                .map(|h| h.join("Music").to_string_lossy().to_string())
                .unwrap_or_default(),
            cover: None,
            cover_open: false,
            new_playlist: None,
            rename: None,
            confirm_delete: false,
            export_playlist: None,
            export_path: String::new(),
            include_audio: true,
            import_path: crate::model::imports_dir().to_string_lossy().to_string(),
            transfer_busy: false,
        };
        app.refilter();
        if !app.core.state.folders.is_empty() {
            app.core.request_scan();
        }
        let task = Task::batch([app.scan_task(), app.cover_task()]);
        (app, task)
    }

    /// Подгружает обложку, когда сменился трек (в фоне: чтение тегов и декодирование).
    fn cover_task(&mut self) -> Task<Message> {
        let Some(track) = self.core.player.as_ref().and_then(|p| p.current()) else {
            return Task::none();
        };
        if self
            .cover
            .as_ref()
            .map(|(id, _)| id == &track.id)
            .unwrap_or(false)
        {
            return Task::none();
        }
        let track_id = track.id.clone();
        let path = track.path.clone();
        self.cover = Some((track_id.clone(), None));
        Task::perform(
            async move {
                let img = crate::cover::load(&path).unwrap_or_else(crate::cover::fallback);
                let small = crate::cover::thumb(&img, 192);
                (track_id, to_handle(img), to_handle(small))
            },
            |(track_id, big, small)| Message::CoverLoaded {
                track_id,
                big,
                small,
            },
        )
    }

    /// Запускает сканирование, если ядро (или пользователь) его запросило.
    fn scan_task(&mut self) -> Task<Message> {
        // Просьба не должна теряться, пока идёт прежний скан (`busy` проверяем первым).
        if self.core.busy || !self.core.take_scan_request() {
            return Task::none();
        }
        self.core.busy = true;
        self.core.status = "Scanning folders…".into();
        let folders = self.core.state.folders.clone();
        let known = self.core.state.tracks.clone();
        let excluded = self.core.state.excluded.clone();
        Task::perform(
            async move { crate::library::scan(&folders, &known, &excluded) },
            Message::Scanned,
        )
    }

    /// Диапазон строк, которые рисуем: `[first, last)`.
    fn window(&self) -> (usize, usize) {
        let total = self.visible.len();
        let first = ((self.scroll_y / ROW_H) as usize).saturating_sub(OVERSCAN);
        let count = (self.view_h / ROW_H) as usize + 2 * OVERSCAN + 1;
        let last = (first + count).min(total);
        (first.min(last), last)
    }

    /// Новый текст запроса: слова в нижнем регистре + пересчёт результатов.
    fn set_search(&mut self, text: String) {
        self.terms = text.split_whitespace().map(str::to_lowercase).collect();
        self.search = text;
        self.refilter();
    }

    fn refilter(&mut self) {
        self.refilter_tracks(true);
        self.refilter_plugins();
    }

    /// Пересчитывает список видимых треков (плейлист + поиск).
    /// Совпадения по названию идут раньше остальных.
    fn refilter_tracks(&mut self, reset_scroll: bool) {
        if self.hay_dirty || self.hay.len() != self.core.state.tracks.len() {
            self.hay = self
                .core
                .state
                .tracks
                .iter()
                .map(|t| {
                    let mut text = t.title.to_lowercase();
                    let title_len = text.len();
                    for part in [t.artist.as_str(), t.album.as_str()] {
                        text.push('\u{1}');
                        text.push_str(&part.to_lowercase());
                    }
                    text.push('\u{1}');
                    text.push_str(&t.file_name().to_lowercase());
                    Hay { text, title_len }
                })
                .collect();
            self.hay_dirty = false;
        }

        // Плейлист могли удалить, пока он был открыт: тогда показываем всю библиотеку.
        if self
            .selected
            .as_ref()
            .is_some_and(|id| self.core.state.playlist(id).is_none())
        {
            self.selected = None;
        }

        let state = &self.core.state;
        // Плейлист показывается в своём порядке (его можно менять в режиме правки).
        let candidates: Option<Vec<usize>> = self
            .selected
            .as_ref()
            .and_then(|id| state.playlist(id))
            .map(|p| {
                let index: std::collections::HashMap<&str, usize> = state
                    .tracks
                    .iter()
                    .enumerate()
                    .map(|(i, t)| (t.id.as_str(), i))
                    .collect();
                p.track_ids
                    .iter()
                    .filter_map(|id| index.get(id.as_str()).copied())
                    .collect()
            });
        let mut first: Vec<usize> = Vec::new();
        let mut rest: Vec<usize> = Vec::new();
        let mut consider = |i: usize| {
            if self.terms.is_empty() {
                first.push(i);
                return;
            }
            let hay = &self.hay[i];
            if !self.terms.iter().all(|q| hay.text.contains(q.as_str())) {
                return;
            }
            let title = &hay.text[..hay.title_len];
            if self.terms.iter().all(|q| title.contains(q.as_str())) {
                first.push(i);
            } else {
                rest.push(i);
            }
        };
        match &candidates {
            Some(list) => list.iter().for_each(|&i| consider(i)),
            None => (0..state.tracks.len()).for_each(&mut consider),
        }
        first.extend(rest);
        self.visible = first;
        if reset_scroll {
            self.scroll_y = 0.0;
            self.scroll_reset = true;
        } else {
            // Список мог стать короче: смещение не должно уходить за его конец,
            // иначе окно строк окажется пустым до следующей прокрутки.
            let max = (self.visible.len() as f32 * ROW_H - self.view_h).max(0.0);
            self.scroll_y = self.scroll_y.min(max);
        }
    }

    /// Плагины, подходящие под запрос: по названию, id, описанию, автору,
    /// подписям настроек и команд. Совпадения по названию идут первыми.
    fn refilter_plugins(&mut self) {
        let plugins = &self.core.host.plugins;
        if self.terms.is_empty() {
            self.plugin_hits = (0..plugins.len()).collect();
            return;
        }
        let mut scored: Vec<(u8, usize)> = Vec::new();
        for (idx, plugin) in plugins.iter().enumerate() {
            let m = &plugin.manifest;
            let name = format!("{} {}", m.name, m.id).to_lowercase();
            let mut all = format!("{name} {} {}", m.description, m.author).to_lowercase();
            for d in &m.settings {
                all.push(' ');
                all.push_str(&d.label.to_lowercase());
                all.push(' ');
                all.push_str(&d.key.to_lowercase());
            }
            for cmd in &m.commands {
                all.push(' ');
                all.push_str(&cmd.label.to_lowercase());
                all.push(' ');
                all.push_str(&cmd.name.to_lowercase());
            }
            if self.terms.iter().all(|q| name.contains(q.as_str())) {
                scored.push((0, idx));
            } else if self.terms.iter().all(|q| all.contains(q.as_str())) {
                scored.push((1, idx));
            }
        }
        scored.sort_by_key(|(score, idx)| (*score, *idx));
        self.plugin_hits = scored.into_iter().map(|(_, idx)| idx).collect();
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let tracks_before = self.core.state.tracks.len();
        let playlists_before = self.core.state.playlists.len();
        match message {
            Message::Tick => {
                if let Some(player) = &mut self.core.player {
                    player.tick();
                }
                self.core.pump();
            }
            Message::Noop => {}
            Message::Page(page) => self.page = page,
            Message::Search(q) => {
                // Поиск работает на странице плагинов (фильтр) и в библиотеке;
                // с остальных страниц и из обложки переходим в библиотеку.
                if !q.trim().is_empty() {
                    self.cover_open = false;
                    if self.page != Page::Plugins {
                        self.page = Page::Library;
                    }
                }
                self.set_search(q);
            }
            Message::ClearSearch => self.set_search(String::new()),
            Message::FocusSearch => {
                self.cover_open = false;
                return text_input::focus(text_input::Id::new(SEARCH_ID));
            }
            Message::ToggleEdit => {
                self.edit_mode = !self.edit_mode;
                self.drag = None;
                self.tag_edit = None;
                self.confirm_remove = None;
            }
            Message::DragStart(pos) => {
                // Порядок меняем только в полном списке: в результатах поиска
                // соседние строки не соседи в библиотеке.
                if self.terms.is_empty() {
                    self.drag = Some(Drag {
                        from: pos,
                        y0: None,
                        to: pos,
                    });
                }
            }
            Message::DragMove(y) => {
                let last = self.visible.len().max(1) as i64 - 1;
                if let Some(d) = &mut self.drag {
                    let y0 = *d.y0.get_or_insert(y);
                    let steps = ((y - y0) / ROW_H).round() as i64;
                    d.to = (d.from as i64 + steps).clamp(0, last) as usize;
                }
            }
            Message::DragEnd => {
                if let Some(d) = self.drag.take() {
                    if d.y0.is_some() && d.to != d.from && self.terms.is_empty() {
                        let moved = match self.selected.clone() {
                            Some(pid) => self.core.move_in_playlist_to(&pid, d.from, d.to),
                            None => self.core.move_track_to(d.from, d.to),
                        };
                        if moved {
                            // Порядок самих треков меняется только в общем списке;
                            // у плейлиста — лишь порядок id в нём.
                            self.hay_dirty |= self.selected.is_none();
                            self.refilter_tracks(false);
                        }
                    }
                }
            }
            Message::RemoveAsk(id) => self.confirm_remove = Some(id),
            Message::RemoveTrack(id) => {
                self.confirm_remove = None;
                self.core.remove_track(&id);
                self.core.status = "Track removed from the library (file not deleted)".into();
                self.hay_dirty = true;
                self.refilter_tracks(false);
            }
            Message::TagStart(id) => {
                if let Some(track) = self.core.state.track(&id).cloned() {
                    self.tag_edit = Some(TagEdit {
                        title: track.title.clone(),
                        artist: track.artist.clone(),
                        album: track.album.clone(),
                        cover_path: String::new(),
                        track,
                        busy: false,
                    });
                    self.confirm_remove = None;
                }
            }
            Message::TagTitle(v) => {
                if let Some(e) = &mut self.tag_edit {
                    e.title = v;
                }
            }
            Message::TagArtist(v) => {
                if let Some(e) = &mut self.tag_edit {
                    e.artist = v;
                }
            }
            Message::TagAlbum(v) => {
                if let Some(e) = &mut self.tag_edit {
                    e.album = v;
                }
            }
            Message::TagCoverPath(v) => {
                if let Some(e) = &mut self.tag_edit {
                    e.cover_path = v;
                }
            }
            Message::TagCancel => self.tag_edit = None,
            Message::TagSave => {
                if let Some(e) = &mut self.tag_edit {
                    if !e.busy {
                        e.busy = true;
                        let (track, title, artist, album, cover_path) = (
                            e.track.clone(),
                            e.title.clone(),
                            e.artist.clone(),
                            e.album.clone(),
                            (!e.cover_path.trim().is_empty())
                                .then(|| e.cover_path.trim().to_string()),
                        );
                        self.core.status = "Writing tags…".into();
                        return Task::perform(
                            async move { engine::write_tags(track, title, artist, album, cover_path) },
                            Message::TagSaved,
                        );
                    }
                }
            }
            Message::TagSaved(result) => match result {
                Ok(track) => {
                    // Обложка перечитывается (в конце `update`), только если правили
                    // трек, который сейчас играет.
                    if self.cover.as_ref().is_some_and(|(id, _)| *id == track.id) {
                        self.cover = None;
                    }
                    self.core.replace_track(track);
                    self.tag_edit = None;
                    self.core.status = "Tags and cover saved".into();
                    self.hay_dirty = true;
                    self.refilter_tracks(false);
                }
                Err(e) => {
                    if let Some(edit) = &mut self.tag_edit {
                        edit.busy = false;
                    }
                    self.core.status = e;
                }
            },
            Message::RestoreRemoved => self.core.restore_removed(),
            Message::Select(id) => {
                self.selected = id;
                self.refilter();
            }
            Message::PlayAt(pos) => {
                let tracks: Vec<Track> = self
                    .visible
                    .iter()
                    .filter_map(|&i| self.core.state.tracks.get(i).cloned())
                    .collect();
                match &mut self.core.player {
                    Some(player) => player.play_queue(tracks, pos),
                    None => self.core.status = "Audio output unavailable".into(),
                }
            }
            Message::Rescan => self.core.request_scan(),
            Message::Scanned(tracks) => {
                self.core.apply_scan(tracks);
                // Номера строк могли сдвинуться: начатое перетаскивание отменяем.
                self.drag = None;
                self.hay_dirty = true;
                // Скан идёт и по просьбе плагинов, и после правки папок: прокрутку
                // не сбрасываем, чтобы список не прыгал наверх.
                self.refilter_tracks(false);
            }
            Message::Scrolled(offset) => self.scroll_y = offset,
            Message::Resized(height) => self.view_h = height,

            Message::Toggle => {
                if let Some(player) = &mut self.core.player {
                    player.toggle();
                }
            }
            Message::Next => {
                if let Some(player) = &mut self.core.player {
                    player.next();
                }
            }
            Message::Prev => {
                if let Some(player) = &mut self.core.player {
                    player.prev();
                }
            }
            Message::Seeking(pos) => self.seeking = Some(pos),
            Message::SeekReleased => {
                if let (Some(pos), Some(player)) = (self.seeking.take(), &mut self.core.player) {
                    player.seek(pos);
                }
            }
            Message::Volume(v) => {
                if let Some(player) = &mut self.core.player {
                    player.set_volume(v);
                }
            }
            Message::VolumeReleased => self.core.mark_settings_dirty(),
            Message::CycleRepeat => {
                if let Some(player) = &mut self.core.player {
                    player.repeat = player.repeat.next();
                }
            }
            Message::ToggleShuffle => {
                if let Some(player) = &mut self.core.player {
                    player.shuffle = !player.shuffle;
                }
            }
            Message::SeekBy(delta) => {
                if let Some(player) = &mut self.core.player {
                    let target = player.position + delta;
                    player.seek(target);
                }
            }
            Message::VolumeBy(delta) => {
                if let Some(player) = &mut self.core.player {
                    let volume = player.volume + delta;
                    player.set_volume(volume);
                    self.core.mark_settings_dirty();
                }
            }
            Message::CoverLoaded {
                track_id,
                big,
                small,
            } => {
                if self
                    .cover
                    .as_ref()
                    .map(|(id, _)| id == &track_id)
                    .unwrap_or(false)
                {
                    self.cover = Some((track_id, Some((big, small))));
                }
            }
            Message::ToggleCover => self.cover_open = !self.cover_open,
            Message::Escape => {
                if self.cover_open {
                    self.cover_open = false;
                } else if self.drag.is_some() {
                    self.drag = None;
                } else if self.tag_edit.is_some() || self.confirm_remove.is_some() {
                    self.tag_edit = None;
                    self.confirm_remove = None;
                } else if self.new_playlist.is_some()
                    || self.rename.is_some()
                    || self.confirm_delete
                {
                    self.new_playlist = None;
                    self.rename = None;
                    self.confirm_delete = false;
                } else if !self.search.is_empty() {
                    self.set_search(String::new());
                }
            }

            Message::NewPlaylistStart => {
                self.new_playlist = Some(String::new());
                self.rename = None;
                self.confirm_delete = false;
                return text_input::focus(text_input::Id::new(NEW_PLAYLIST_ID));
            }
            Message::NewPlaylistInput(text) => self.new_playlist = Some(text),
            Message::NewPlaylistSubmit => {
                if let Some(name) = self.new_playlist.take() {
                    let name = name.trim().to_string();
                    if !name.is_empty() {
                        let id = self.core.create_playlist(&name);
                        self.selected = Some(id);
                        self.page = Page::Library;
                        self.refilter();
                    }
                }
            }
            Message::RenameStart => {
                let name = self
                    .selected
                    .as_ref()
                    .and_then(|id| self.core.state.playlist(id))
                    .map(|p| p.name.clone());
                if let Some(name) = name {
                    self.rename = Some(name);
                    self.new_playlist = None;
                    self.confirm_delete = false;
                    return text_input::focus(text_input::Id::new(RENAME_ID));
                }
            }
            Message::RenameInput(text) => self.rename = Some(text),
            Message::RenameSubmit => {
                if let (Some(name), Some(id)) = (self.rename.take(), self.selected.clone()) {
                    self.core.rename_playlist(&id, &name);
                }
            }
            Message::DeleteAsk => {
                self.confirm_delete = true;
                self.rename = None;
            }
            Message::DeleteConfirm => {
                if let Some(id) = self.selected.take() {
                    self.core.delete_playlist(&id);
                    self.confirm_delete = false;
                    self.refilter();
                }
            }
            Message::AddToPlaylist { playlist, track } => {
                let name = self
                    .core
                    .state
                    .playlist(&playlist)
                    .map(|p| p.name.clone())
                    .unwrap_or_default();
                self.core.status = if self.core.add_to_playlist(&playlist, &track) {
                    format!("Added to '{name}'")
                } else {
                    format!("Already in '{name}'")
                };
            }
            Message::RemoveFromPlaylist(track) => {
                if let Some(id) = self.selected.clone() {
                    self.core.remove_from_playlist(&id, &track);
                    self.refilter_tracks(false);
                }
            }

            Message::ExportSelect(id) => {
                if let Some(playlist) = self.core.state.playlist(&id) {
                    self.export_path = engine::default_export_path(&playlist.name, true);
                }
                self.export_playlist = Some(id);
            }
            Message::ExportPath(text) => self.export_path = text,
            Message::ExportAudio(on) => self.include_audio = on,
            Message::ExportRun(zip) => {
                let playlist = self
                    .export_playlist
                    .as_ref()
                    .and_then(|id| self.core.state.playlist(id))
                    .cloned();
                if let (Some(playlist), false) = (playlist, self.transfer_busy) {
                    let tracks = self.core.state.tracks_of(&playlist);
                    let path = self.export_path.clone();
                    let include_audio = self.include_audio;
                    self.transfer_busy = true;
                    self.core.status = "Exporting…".into();
                    return Task::perform(
                        async move { engine::run_export(&path, playlist, tracks, zip, include_audio) },
                        Message::Exported,
                    );
                }
            }
            Message::Exported(result) => {
                self.transfer_busy = false;
                self.core.status = match result {
                    Ok(path) => format!("Exported: {path}"),
                    Err(e) => format!("Export error: {e}"),
                };
            }
            Message::ImportPath(text) => self.import_path = text,
            Message::ImportRun => {
                if !self.transfer_busy {
                    let path = self.import_path.clone();
                    let known = self.core.state.tracks.clone();
                    self.transfer_busy = true;
                    self.core.status = "Importing…".into();
                    return Task::perform(
                        async move { engine::prepare_import(&path, &known) },
                        Message::Imported,
                    );
                }
            }
            Message::Imported(result) => {
                self.transfer_busy = false;
                match result {
                    Ok(outcome) => {
                        let (id, status) = self.core.apply_import(outcome);
                        self.core.status = status;
                        self.selected = Some(id);
                        self.page = Page::Library;
                        self.refilter();
                    }
                    Err(e) => self.core.status = e,
                }
            }
            Message::OpenExports => open_folder(&crate::model::exports_dir()),

            Message::FolderInput(text) => self.folder_input = text,
            Message::AddFolder => {
                let path = self.folder_input.clone();
                if let Err(reason) = self.core.add_folder(&path) {
                    self.core.status = reason;
                }
            }
            Message::AddMusicDir => {
                if let Some(music) = dirs::home_dir().map(|h| h.join("Music")) {
                    let path = music.to_string_lossy().to_string();
                    self.folder_input = path.clone();
                    if let Err(reason) = self.core.add_folder(&path) {
                        self.core.status = reason;
                    }
                }
            }
            Message::RemoveFolder(path) => self.core.remove_folder(&path),

            Message::PluginToggle(idx, on) => self.core.set_plugin_enabled(idx, on),
            Message::PluginConfig { id, key, value } => {
                self.core.set_plugin_config(&id, &key, value)
            }
            Message::PluginStep { id, key, delta } => {
                let new_value = self.core.host.position(&id).and_then(|idx| {
                    let manifest = &self.core.host.plugins[idx].manifest;
                    let def = manifest.settings.iter().find(|d| d.key == key)?;
                    let user = self
                        .core
                        .settings
                        .plugins
                        .get(&id)
                        .map(|e| e.config.clone())
                        .unwrap_or_default();
                    let config = manifest.effective_config(&user);
                    let current = config.get(&key).and_then(Value::as_i64).unwrap_or(0);
                    Some(def.coerce(&json!(current.saturating_add(delta))))
                });
                if let Some(value) = new_value {
                    self.core.set_plugin_config(&id, &key, value);
                }
            }
            Message::PluginCommand(idx, name) => self.core.plugin_command(idx, &name),
            Message::PluginsRescan => {
                self.core.rescan_plugins();
                self.refilter_plugins();
            }
        }

        // Плагины или папки могли изменить библиотеку.
        if self.core.state.tracks.len() != tracks_before
            || self.core.state.playlists.len() != playlists_before
        {
            self.drag = None;
            self.refilter_tracks(false);
        }
        // Ошибка воспроизведения (битый файл, нет устройства) попадает в статус сразу
        // и один раз, а не при каждом тике поверх других сообщений.
        if let Some(error) = self.core.player.as_mut().and_then(|p| p.error.take()) {
            self.core.status = error;
        }
        // Список треков исчезает из дерева виджетов (другая страница, обложка) и при
        // возвращении создаётся с нулевым смещением: счётчик прокрутки должен совпасть.
        if self.page != Page::Library || self.cover_open {
            self.scroll_y = 0.0;
        }
        let mut tasks = vec![self.scan_task(), self.cover_task()];
        if std::mem::take(&mut self.scroll_reset) {
            // Виджет списка помнит смещение сам: без этого после нового поиска
            // окно строк считалось бы от нуля, а список был бы прокручен.
            tasks.push(iced::widget::scrollable::snap_to(
                iced::widget::scrollable::Id::new(LIST_ID),
                iced::widget::scrollable::RelativeOffset::START,
            ));
        }
        Task::batch(tasks)
    }

    fn subscription(&self) -> Subscription<Message> {
        let playing = self.core.player.as_ref().is_some_and(|p| p.playing);
        // Быстрый тик нужен, пока идёт музыка, работают плагины или ждёт записи
        // отложенное сохранение (оно выполняется в `pump`).
        let every = if playing || self.core.host.any_ready() || self.core.has_pending_saves() {
            Duration::from_millis(250)
        } else {
            Duration::from_secs(2)
        };
        let mut subs = vec![
            iced::time::every(every).map(|_| Message::Tick),
            // Не срабатывает, пока фокус в поле ввода: оно забирает клавиши себе.
            keyboard::on_key_press(on_key),
            // Высота окна нужна, чтобы список рисовал достаточно строк.
            window::resize_events().map(|(_, size)| Message::Resized(size.height)),
        ];
        // Мышь слушаем только во время перетаскивания: иначе каждое движение
        // курсора перерисовывало бы окно.
        if self.drag.is_some() {
            subs.push(iced::event::listen_with(on_event));
        }
        Subscription::batch(subs)
    }

    fn theme(&self) -> Theme {
        let p = pal();
        Theme::custom(
            "mpt".to_string(),
            iced::theme::Palette {
                background: c(p.bg),
                text: c(p.text),
                primary: c(p.accent),
                success: c(p.accent2),
                danger: c(p.danger),
            },
        )
    }

    // ───────────── отображение ─────────────

    fn view(&self) -> Element<'_, Message> {
        let body: Element<'_, Message> = if self.cover_open {
            self.cover_page()
        } else {
            match self.page {
                Page::Library => row![self.sidebar(), self.track_list()]
                    .height(Length::Fill)
                    .into(),
                Page::Folders => self.folders_page(),
                Page::Transfer => self.transfer_page(),
                Page::Plugins => self.plugins_page(),
            }
        };
        let base: Element<'_, Message> =
            column![self.top_bar(), body, self.player_bar(), self.status_bar()].into();
        if let Some(edit) = &self.tag_edit {
            // Нажатие внутри окна «съедает» вложенная область: иначе оно дошло бы до
            // подложки и закрыло бы окно вместе с несохранёнными правками.
            let modal = mouse_area(
                container(self.tag_panel(edit))
                    .width(Length::Fixed(620.0))
                    .height(Length::Shrink)
                    .padding(18)
                    .style(card_style),
            )
            .on_press(Message::Noop);
            // Подложка закрывает окно по клику и не пропускает колесо мыши к списку под ней.
            let backdrop = mouse_area(
                container(modal)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill)
                    .padding(24)
                    .style(|_: &Theme| container::Style {
                        background: Some(Background::Color(Color::from_rgba(
                            0.02, 0.03, 0.03, 0.82,
                        ))),
                        ..Default::default()
                    }),
            )
            .on_press(Message::TagCancel)
            .on_scroll(|_| Message::Noop);
            stack![base, backdrop]
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        } else {
            base
        }
    }

    /// Обложка на всё окно; клик или Esc закрывают.
    fn cover_page(&self) -> Element<'_, Message> {
        let picture: Element<'_, Message> = match &self.cover {
            Some((_, Some((big, _)))) => image(big.clone())
                .content_fit(ContentFit::Contain)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            _ => text("loading cover…").color(c(pal().muted)).into(),
        };
        let (title, artist) = self
            .core
            .player
            .as_ref()
            .and_then(|p| p.current())
            .map(|t| (t.title.clone(), t.artist.clone()))
            .unwrap_or_default();
        button(
            column![
                container(picture)
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .center_x(Length::Fill)
                    .center_y(Length::Fill),
                text(title).size(18),
                text(artist).size(12).color(c(pal().muted)),
            ]
            .spacing(6)
            .align_x(Alignment::Center),
        )
        .on_press(Message::ToggleCover)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(16)
        .style(|_: &Theme, _| button::Style {
            background: Some(Background::Color(c(pal().bg))),
            text_color: c(pal().text),
            border: square_border(Color::TRANSPARENT, 0.0),
            ..Default::default()
        })
        .into()
    }

    fn top_bar(&self) -> Element<'_, Message> {
        let tab = |label: &'static str, page: Page| {
            nav_button(label.to_string(), self.page == page, Message::Page(page))
        };
        let mut search = row![
            text_input("Search tracks and plugins…  ( / )", &self.search)
                .id(text_input::Id::new(SEARCH_ID))
                .on_input(Message::Search)
                .padding(6)
                .style(input_style)
                .width(Length::Fill)
        ]
        .spacing(4)
        .align_y(Alignment::Center)
        .width(360);
        if !self.search.is_empty() {
            search = search.push(flat_button("X", Some(Message::ClearSearch)));
        }
        container(
            row![
                text("mpt.mp").font(PIXEL).size(22).color(c(pal().accent)),
                horizontal_space().width(16),
                tab("LIBRARY", Page::Library),
                tab("FOLDERS", Page::Folders),
                tab("TRANSFER", Page::Transfer),
                tab(
                    if self.core.host.plugins.is_empty() {
                        "PLUGINS"
                    } else {
                        "PLUGINS •"
                    },
                    Page::Plugins
                ),
                horizontal_space(),
                search,
            ]
            .spacing(4)
            .align_y(Alignment::Center),
        )
        .padding([6, 12])
        .width(Length::Fill)
        .style(panel_style)
        .into()
    }

    fn sidebar(&self) -> Element<'_, Message> {
        let state = &self.core.state;
        let mut list = column![
            text("LIBRARY").size(12).color(c(pal().muted)),
            nav_button(
                format!("All tracks  {}", state.tracks.len()),
                self.selected.is_none(),
                Message::Select(None)
            ),
            vertical_space().height(10),
            row![
                text("PLAYLISTS").size(12).color(c(pal().muted)),
                horizontal_space(),
                plus_button(Message::NewPlaylistStart),
            ]
            .align_y(Alignment::Center),
        ]
        .spacing(4);
        for playlist in &state.playlists {
            list = list.push(nav_button(
                format!("{}  {}", playlist.name, playlist.track_ids.len()),
                self.selected.as_deref() == Some(playlist.id.as_str()),
                Message::Select(Some(playlist.id.clone())),
            ));
        }
        // Новый плейлист.
        if let Some(name) = &self.new_playlist {
            list = list.push(
                text_input("name…", name)
                    .id(text_input::Id::new(NEW_PLAYLIST_ID))
                    .on_input(Message::NewPlaylistInput)
                    .on_submit(Message::NewPlaylistSubmit)
                    .padding(6)
                    .style(input_style),
            );
        }

        // Действия над выбранным плейлистом.
        if self.selected.is_some() {
            list = list.push(vertical_space().height(6));
            if let Some(name) = &self.rename {
                list = list.push(
                    text_input("name…", name)
                        .id(text_input::Id::new(RENAME_ID))
                        .on_input(Message::RenameInput)
                        .on_submit(Message::RenameSubmit)
                        .padding(6)
                        .style(input_style),
                );
            } else if self.confirm_delete {
                list = list
                    .push(text("Delete playlist?").size(12).color(c(pal().danger)))
                    .push(
                        row![
                            flat_button("YES", Some(Message::DeleteConfirm)),
                            flat_button("NO", Some(Message::Escape)),
                        ]
                        .spacing(6),
                    );
            } else {
                list = list.push(
                    row![
                        flat_button("RENAME", Some(Message::RenameStart)),
                        flat_button("DELETE", Some(Message::DeleteAsk)),
                    ]
                    .spacing(6),
                );
            }
        }

        container(vscroll(list.padding(12)))
            .width(220)
            .height(Length::Fill)
            .style(panel_style)
            .into()
    }

    fn track_list(&self) -> Element<'_, Message> {
        let searching = !self.terms.is_empty();
        let total = self.visible.len();
        let (first, last) = self.window();

        let current = self
            .core
            .player
            .as_ref()
            .and_then(|p| p.current())
            .map(|t| t.id.as_str());

        let playlist_choices: Vec<PlaylistChoice> = self
            .core
            .state
            .playlists
            .iter()
            .map(|p| PlaylistChoice {
                id: p.id.clone(),
                name: p.name.clone(),
            })
            .collect();

        // Рисуем только окно строк; остальное заменяют распорки фиксированной высоты.
        let mut rows = column![vertical_space().height(first as f32 * ROW_H)];
        for pos in first..last {
            let Some(track) = self.core.state.tracks.get(self.visible[pos]) else {
                continue;
            };
            let mark = match &self.drag {
                Some(d) if pos == d.from => Mark::Dragged,
                Some(d) if pos == d.to && d.y0.is_some() => Mark::Target,
                _ => Mark::None,
            };
            let edit = self.edit_mode.then(|| EditCtx {
                can_drag: self.terms.is_empty(),
                confirm: self.confirm_remove.as_deref() == Some(track.id.as_str()),
                mark,
            });
            rows = rows.push(track_row(
                pos,
                track,
                current == Some(track.id.as_str()),
                self.selected.is_some(),
                &playlist_choices,
                edit,
            ));
        }
        rows = rows.push(vertical_space().height(total.saturating_sub(last) as f32 * ROW_H));

        // Список всегда в `scrollable` (иначе при открытии окна тегов виджет терял бы
        // смещение); колесо мыши за модальным окном гасит его подложка.
        let body: Element<'_, Message> = if total == 0 {
            let message = if searching {
                if self.plugin_hits.is_empty() {
                    "Nothing found"
                } else {
                    "No tracks found"
                }
            } else if self.selected.is_some() {
                "This playlist is empty"
            } else if self.core.state.folders.is_empty() {
                "Add a music folder on the FOLDERS page"
            } else if self.core.busy {
                "Scanning…"
            } else {
                "No tracks found in the music folders"
            };
            container(text(message).color(c(pal().muted)))
                .padding(20)
                .into()
        } else {
            vscroll(rows)
                .id(iced::widget::scrollable::Id::new(LIST_ID))
                .height(Length::Fill)
                .on_scroll(|viewport| Message::Scrolled(viewport.absolute_offset().y))
                .into()
        };

        let mut page = column![]
            .spacing(8)
            .padding(12)
            .width(Length::Fill)
            .height(Length::Fill);
        page = page.push(
            row![
                text(format!(
                    "TRACKS{}  {total}",
                    if self.selected.is_some() {
                        " IN PLAYLIST"
                    } else {
                        ""
                    }
                ))
                .size(12)
                .color(c(pal().muted)),
                flat_button(
                    if self.core.busy {
                        "SCANNING…"
                    } else {
                        "REFRESH"
                    },
                    (!self.core.busy).then_some(Message::Rescan),
                ),
                horizontal_space(),
                edit_button(self.edit_mode),
            ]
            .spacing(10)
            .align_y(Alignment::Center),
        );
        page = page.push(body);
        // Плагины — только после треков.
        if searching && !self.plugin_hits.is_empty() {
            page = page.push(self.plugin_results());
        }
        page.into()
    }

    /// Центральное модальное окно редактирования тегов и обложки.
    fn tag_panel<'a>(&'a self, edit: &'a TagEdit) -> Element<'a, Message> {
        let field = |label: &'static str, value: &'a str, on_input: fn(String) -> Message| {
            column![
                text(label).size(11).color(c(pal().muted)),
                text_input("", value)
                    .on_input(on_input)
                    .on_submit(Message::TagSave)
                    .padding(9)
                    .style(input_style)
                    .width(Length::Fill),
            ]
            .spacing(4)
        };
        let save_label = if edit.busy {
            "SAVING…"
        } else {
            "SAVE CHANGES"
        };
        let panel = column![
            row![
                column![
                    text("EDIT TRACK").size(15).color(c(pal().accent)),
                    text(edit.track.file_name()).size(11).color(c(pal().muted)),
                ]
                .spacing(3),
                horizontal_space(),
                close_button(Message::TagCancel),
            ]
            .align_y(Alignment::Center),
            field("TITLE", &edit.title, Message::TagTitle),
            field("ARTIST", &edit.artist, Message::TagArtist),
            field("ALBUM", &edit.album, Message::TagAlbum),
            column![
                text("COVER IMAGE").size(11).color(c(pal().muted)),
                text_input("Path to image (PNG / JPEG)", &edit.cover_path)
                    .on_input(Message::TagCoverPath)
                    .padding(9)
                    .style(input_style)
                    .width(Length::Fill),
                text("Paste the image file path. Leave empty to keep the current cover.")
                    .size(10)
                    .color(c(pal().muted)),
            ]
            .spacing(4),
            row![
                text("Changes are written into the audio file.")
                    .size(10)
                    .color(c(pal().muted)),
                horizontal_space(),
                flat_button("CANCEL", Some(Message::TagCancel)),
                flat_button(save_label, (!edit.busy).then_some(Message::TagSave)),
            ]
            .align_y(Alignment::Center)
            .spacing(8),
        ]
        .spacing(12);
        // Рамку и фон даёт само модальное окно: вторая, внутренняя, не нужна.
        panel.into()
    }

    /// Плагины, найденные поиском: компактный блок под списком треков.
    fn plugin_results(&self) -> Element<'_, Message> {
        let hits = &self.plugin_hits;
        let mut block = column![text(format!("PLUGINS  {}", hits.len()))
            .size(12)
            .color(c(pal().accent2))]
        .spacing(6);
        for &idx in hits.iter().take(PLUGIN_HITS_SHOWN) {
            if let Some(plugin) = self.core.host.plugins.get(idx) {
                block = block.push(self.plugin_hit(idx, plugin));
            }
        }
        if hits.len() > PLUGIN_HITS_SHOWN {
            block = block.push(flat_button(
                "ALL PLUGIN RESULTS",
                Some(Message::Page(Page::Plugins)),
            ));
        }
        container(block.padding(10))
            .width(Length::Fill)
            .style(card_style)
            .into()
    }

    /// Строка плагина в результатах поиска: имя, описание, состояние, переключатель.
    fn plugin_hit<'a>(&'a self, idx: usize, plugin: &'a Plugin) -> Element<'a, Message> {
        let manifest = &plugin.manifest;
        let enabled = self.core.plugin_enabled(&manifest.id);
        let (state_text, state_color) = run_state_label(plugin.state);
        let mut info = column![row![
            text(&manifest.name).size(13),
            text(format!("v{}", manifest.version))
                .size(11)
                .color(c(pal().muted)),
        ]
        .spacing(8)
        .align_y(Alignment::Center)]
        .spacing(2);
        if !manifest.description.is_empty() {
            info = info.push(
                text(&manifest.description)
                    .size(11)
                    .color(c(pal().muted))
                    .wrapping(iced::widget::text::Wrapping::None),
            );
        }
        row![
            container(info).width(Length::Fill).clip(true),
            text(state_text).size(11).color(c(state_color)),
            toggle_button(enabled, Message::PluginToggle(idx, !enabled)),
            flat_button("OPEN", Some(Message::Page(Page::Plugins))),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .into()
    }

    fn transfer_page(&self) -> Element<'_, Message> {
        let state = &self.core.state;
        let mut export = column![
            text("EXPORT").size(12).color(c(pal().accent)),
            text("PLAYLIST").size(11).color(c(pal().muted)),
        ]
        .spacing(6);
        if state.playlists.is_empty() {
            export = export.push(text("no playlists yet").size(12).color(c(pal().muted)));
        }
        let mut choices = column![].spacing(4);
        for playlist in &state.playlists {
            choices = choices.push(nav_button(
                playlist.name.clone(),
                self.export_playlist.as_deref() == Some(playlist.id.as_str()),
                Message::ExportSelect(playlist.id.clone()),
            ));
        }
        let can_export = self.export_playlist.is_some() && !self.transfer_busy;
        export = export
            .push(choices)
            .push(text("FILE").size(11).color(c(pal().muted)))
            .push(
                text_input("file path", &self.export_path)
                    .on_input(Message::ExportPath)
                    .padding(8)
                    .style(input_style),
            )
            .push(
                row![
                    chip_button(
                        "AUDIO FILES IN ZIP".to_string(),
                        self.include_audio,
                        Message::ExportAudio(!self.include_audio)
                    ),
                    horizontal_space(),
                    flat_button(
                        "EXPORT .M3U",
                        can_export.then_some(Message::ExportRun(false))
                    ),
                    flat_button(
                        "EXPORT .ZIP",
                        can_export.then_some(Message::ExportRun(true))
                    ),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            )
            .push(
                text(format!("folder: {}", crate::model::exports_dir().display()))
                    .font(Font::MONOSPACE)
                    .size(11)
                    .color(c(pal().muted)),
            )
            .push(flat_button(
                "OPEN EXPORTS FOLDER",
                Some(Message::OpenExports),
            ));

        let import = column![
            text("IMPORT").size(12).color(c(pal().accent2)),
            text("PATH TO .M3U / .M3U8 / .ZIP")
                .size(11)
                .color(c(pal().muted)),
            text_input("file path", &self.import_path)
                .on_input(Message::ImportPath)
                .on_submit(Message::ImportRun)
                .padding(8)
                .style(input_style),
            flat_button(
                if self.transfer_busy {
                    "WORKING…"
                } else {
                    "IMPORT FILE"
                },
                (!self.transfer_busy).then_some(Message::ImportRun)
            ),
            text("import creates a new playlist and adds missing tracks")
                .size(11)
                .color(c(pal().muted)),
        ]
        .spacing(6);

        let page = column![
            container(export)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
            container(import)
                .padding(14)
                .width(Length::Fill)
                .style(card_style),
        ]
        .spacing(12)
        .padding(20)
        .max_width(900);
        container(vscroll(page))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn folders_page(&self) -> Element<'_, Message> {
        let mut list = column![text("MUSIC FOLDERS").size(12).color(c(pal().muted))].spacing(6);
        if self.core.state.folders.is_empty() {
            list = list.push(text("no folders added yet").size(12).color(c(pal().muted)));
        }
        for folder in &self.core.state.folders {
            list = list.push(
                row![
                    text(folder).size(13).width(Length::Fill),
                    flat_button("REMOVE", Some(Message::RemoveFolder(folder.clone()))),
                ]
                .align_y(Alignment::Center)
                .spacing(10),
            );
        }
        if !self.core.state.excluded.is_empty() {
            list = list.push(flat_button(
                "RESTORE REMOVED TRACKS",
                Some(Message::RestoreRemoved),
            ));
        }
        list = list
            .push(vertical_space().height(14))
            .push(text("ADD FOLDER").size(12).color(c(pal().muted)))
            .push(
                row![
                    text_input("/path/to/music", &self.folder_input)
                        .on_input(Message::FolderInput)
                        .on_submit(Message::AddFolder)
                        .padding(8)
                        .style(input_style),
                    flat_button("ADD", Some(Message::AddFolder)),
                    flat_button("~/MUSIC", Some(Message::AddMusicDir)),
                ]
                .spacing(8)
                .align_y(Alignment::Center),
            );
        container(vscroll(list.padding(20).max_width(900)))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn plugins_page(&self) -> Element<'_, Message> {
        let host = &self.core.host;
        let searching = !self.terms.is_empty();
        let title = if searching {
            format!(
                "INSTALLED PLUGINS · {} of {} match \"{}\"",
                self.plugin_hits.len(),
                host.plugins.len(),
                self.search.trim()
            )
        } else {
            "INSTALLED PLUGINS".to_string()
        };
        let mut list = column![text(title).size(12).color(c(pal().muted))].spacing(10);
        if host.plugins.is_empty() {
            list = list.push(text("no plugins yet").size(12).color(c(pal().muted)));
        } else if self.plugin_hits.is_empty() {
            list = list.push(
                text("no plugins match the search")
                    .size(12)
                    .color(c(pal().muted)),
            );
        }
        for &idx in &self.plugin_hits {
            if let Some(plugin) = host.plugins.get(idx) {
                list = list.push(self.plugin_card(idx, plugin));
            }
        }
        if !host.invalid.is_empty() {
            list = list.push(text("NOT LOADED").size(12).color(c(pal().muted)));
            for (folder, reason) in &host.invalid {
                list = list.push(
                    text(format!("{folder}: {reason}"))
                        .size(12)
                        .color(c(pal().danger)),
                );
            }
        }
        list = list
            .push(vertical_space().height(8))
            .push(
                text(format!(
                    "plugins folder: {}",
                    crate::model::plugins_dir().display()
                ))
                .font(Font::MONOSPACE)
                .size(12)
                .color(c(pal().muted)),
            )
            .push(
                text("a plugin is a program running with your privileges, without a sandbox; enable only plugins you trust")
                    .size(12)
                    .color(c(pal().warn)),
            )
            .push(flat_button("REFRESH LIST", Some(Message::PluginsRescan)));
        container(vscroll(list.padding(20).max_width(900)))
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn plugin_card<'a>(&'a self, idx: usize, plugin: &'a Plugin) -> Element<'a, Message> {
        let manifest = &plugin.manifest;
        let enabled = self.core.plugin_enabled(&manifest.id);
        let (state_text, state_color) = run_state_label(plugin.state);

        let mut card = column![row![
            container(
                row![
                    text(&manifest.name).size(14),
                    text(format!("v{}", manifest.version))
                        .size(12)
                        .color(c(pal().muted)),
                ]
                .spacing(8)
                .align_y(Alignment::Center)
            )
            .width(Length::Fill),
            text(state_text).size(11).color(c(state_color)),
            toggle_button(enabled, Message::PluginToggle(idx, !enabled)),
        ]
        .spacing(10)
        .align_y(Alignment::Center)]
        .spacing(6);

        if !manifest.description.is_empty() {
            card = card.push(text(&manifest.description).size(12).color(c(pal().muted)));
        }
        let meta: Vec<&str> = [&manifest.author, &manifest.license, &manifest.homepage]
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(String::as_str)
            .collect();
        if !meta.is_empty() {
            card = card.push(text(meta.join(" · ")).size(11).color(c(pal().muted)));
        }
        let perms = if manifest.permissions.is_empty() {
            "none".to_string()
        } else {
            manifest.permissions.join(", ")
        };
        card = card.push(
            text(format!("permissions: {perms}"))
                .size(12)
                .color(c(pal().accent2)),
        );
        if let Some(error) = &plugin.error {
            card = card.push(text(error).size(12).color(c(pal().danger)));
        }
        if let Some((status, level)) = &plugin.status {
            let color = match level {
                Level::Info => pal().text,
                Level::Warn => pal().warn,
                Level::Error => pal().danger,
            };
            card = card.push(text(status).size(12).color(c(color)));
        }

        // Настройки из манифеста.
        let user = self
            .core
            .settings
            .plugins
            .get(&manifest.id)
            .map(|e| e.config.clone())
            .unwrap_or_default();
        let config = manifest.effective_config(&user);
        for def in &manifest.settings {
            let label = if def.label.is_empty() {
                &def.key
            } else {
                &def.label
            };
            let current = config
                .get(&def.key)
                .cloned()
                .unwrap_or_else(|| def.default_value());
            let id = manifest.id.clone();
            let key = def.key.clone();
            let control: Element<'a, Message> = match def.kind.as_str() {
                "bool" => {
                    let on = current.as_bool().unwrap_or(false);
                    toggle_button(
                        on,
                        Message::PluginConfig {
                            id,
                            key,
                            value: json!(!on),
                        },
                    )
                }
                "int" => {
                    let n = current.as_i64().unwrap_or(0);
                    row![
                        flat_button(
                            "-",
                            Some(Message::PluginStep {
                                id: id.clone(),
                                key: key.clone(),
                                delta: -1
                            })
                        ),
                        text(n.to_string()).size(13),
                        flat_button("+", Some(Message::PluginStep { id, key, delta: 1 })),
                    ]
                    .spacing(8)
                    .align_y(Alignment::Center)
                    .into()
                }
                "choice" => {
                    let selected = current.as_str().unwrap_or("").to_string();
                    let mut options = row![].spacing(4);
                    for option in &def.options {
                        options = options.push(nav_button(
                            option.clone(),
                            *option == selected,
                            Message::PluginConfig {
                                id: id.clone(),
                                key: key.clone(),
                                value: json!(option),
                            },
                        ));
                    }
                    options.into()
                }
                _ => {
                    let value = current.as_str().unwrap_or("").to_string();
                    text_input("", &value)
                        .on_input(move |s| Message::PluginConfig {
                            id: id.clone(),
                            key: key.clone(),
                            value: Value::String(s),
                        })
                        .padding(7)
                        .style(input_style)
                        .width(Length::Fill)
                        .into()
                }
            };
            card = card.push(
                row![
                    text(label).size(13).width(Length::FillPortion(2)),
                    container(control).width(Length::FillPortion(3)),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            );
        }

        // Кнопки команд.
        if !manifest.commands.is_empty() {
            let running = plugin.state == RunState::Running;
            let mut commands = row![].spacing(6);
            for cmd in &manifest.commands {
                let label = if cmd.label.is_empty() {
                    &cmd.name
                } else {
                    &cmd.label
                };
                commands = commands.push(flat_button(
                    label,
                    running.then(|| Message::PluginCommand(idx, cmd.name.clone())),
                ));
            }
            card = card.push(commands);
        }

        if plugin.state != RunState::Stopped {
            for line in plugin.log.iter().rev().take(4).rev() {
                card = card.push(
                    text(line)
                        .font(Font::MONOSPACE)
                        .size(11)
                        .color(c(pal().muted)),
                );
            }
        }

        container(card.padding(12))
            .width(Length::Fill)
            .style(card_style)
            .into()
    }

    fn player_bar(&self) -> Element<'_, Message> {
        let (title, artist, playing, pos, dur, vol, repeat, shuffle) = match &self.core.player {
            Some(p) => (
                p.current().map(|t| t.title.clone()).unwrap_or_default(),
                p.current().map(|t| t.artist.clone()).unwrap_or_default(),
                p.playing,
                p.position,
                p.duration,
                p.volume,
                p.repeat,
                p.shuffle,
            ),
            None => (
                String::new(),
                String::new(),
                false,
                0.0,
                0.0,
                0.0,
                RepeatMode::Off,
                false,
            ),
        };
        let shown = self.seeking.unwrap_or(pos);
        let repeat_text = match repeat {
            RepeatMode::Off => "REPEAT",
            RepeatMode::All => "REPEAT: ALL",
            RepeatMode::One => "REPEAT: ONE",
        };

        let thumb: Element<'_, Message> = match &self.cover {
            Some((_, Some((_, small)))) => button(
                image(small.clone())
                    .content_fit(ContentFit::Cover)
                    .width(52)
                    .height(52),
            )
            .on_press(Message::ToggleCover)
            .padding(0)
            .style(|_: &Theme, _| button::Style {
                background: None,
                border: square_border(c(pal().border), 1.0),
                ..Default::default()
            })
            .into(),
            _ => container(vertical_space())
                .width(52)
                .height(52)
                .style(card_style)
                .into(),
        };
        let info = row![
            thumb,
            container(
                column![
                    text(if title.is_empty() {
                        "—".to_string()
                    } else {
                        title
                    })
                    .size(14)
                    .wrapping(iced::widget::text::Wrapping::None),
                    text(artist)
                        .size(12)
                        .color(c(pal().muted))
                        .wrapping(iced::widget::text::Wrapping::None),
                ]
                .spacing(2)
            )
            .width(Length::Fill)
            .clip(true),
        ]
        .spacing(10)
        .align_y(Alignment::Center)
        .width(230);

        let controls = row![
            flat_button("<<", Some(Message::Prev)),
            flat_button(
                if playing { "PAUSE" } else { "PLAY" },
                Some(Message::Toggle)
            ),
            flat_button(">>", Some(Message::Next)),
        ]
        .spacing(6);

        let seek = row![
            text(fmt_secs(shown as u64)).size(12).color(c(pal().muted)),
            slider(
                0.0..=dur.max(1.0),
                shown.min(dur.max(1.0)),
                Message::Seeking
            )
            .on_release(Message::SeekReleased)
            .style(square_slider)
            .step(0.1_f32),
            text(fmt_secs(dur as u64)).size(12).color(c(pal().muted)),
        ]
        .spacing(8)
        .align_y(Alignment::Center)
        .width(Length::Fill);

        let volume = row![
            text("VOL.").size(11).color(c(pal().muted)),
            slider(0.0..=1.0, vol, Message::Volume)
                .on_release(Message::VolumeReleased)
                .style(square_slider)
                .step(0.01_f32)
                .width(90),
        ]
        .spacing(6)
        .align_y(Alignment::Center);

        let modes = row![
            chip_button(
                repeat_text.to_string(),
                repeat != RepeatMode::Off,
                Message::CycleRepeat
            ),
            chip_button("SHUFFLE".to_string(), shuffle, Message::ToggleShuffle),
        ]
        .spacing(6);

        container(
            row![info, controls, seek, volume, modes]
                .spacing(12)
                .align_y(Alignment::Center),
        )
        .padding(12)
        .width(Length::Fill)
        .style(panel_style)
        .into()
    }

    fn status_bar(&self) -> Element<'_, Message> {
        container(
            row![text(&self.core.status).size(12).color(c(pal().muted))]
            .align_y(Alignment::Center),
        )
        .padding([3, 14])
        .width(Length::Fill)
        .into()
    }
}

// ───────────── стили и мелкие виджеты ─────────────

fn square_border(color: Color, width: f32) -> Border {
    Border {
        color,
        width,
        radius: 0.0.into(),
    }
}

/// Поля ввода в общей геометрии интерфейса: квадратная рамка, спокойный фон,
/// акцентная рамка при фокусе и хорошо заметное выделение текста.
fn input_style(_: &Theme, status: text_input::Status) -> text_input::Style {
    let focused = matches!(status, text_input::Status::Focused);
    text_input::Style {
        background: Background::Color(c(pal().panel)),
        border: square_border(
            if focused {
                c(pal().accent)
            } else {
                c(pal().border)
            },
            if focused { 1.5 } else { 1.0 },
        ),
        icon: c(pal().muted),
        placeholder: c(pal().muted),
        value: c(pal().text),
        selection: c(pal().accent),
    }
}

/// Выпадающие списки используют ту же квадратную рамку, что и поля ввода.
fn pick_list_style(_: &Theme, _status: pick_list::Status) -> pick_list::Style {
    pick_list::Style {
        text_color: c(pal().text),
        placeholder_color: c(pal().muted),
        handle_color: c(pal().muted),
        background: Background::Color(c(pal().panel)),
        border: square_border(c(pal().border), 1.0),
    }
}

fn panel_style(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c(pal().panel))),
        // Без рамки: верхняя, боковая и нижняя панели сливаются в одно целое.
        border: square_border(Color::TRANSPARENT, 0.0),
        ..Default::default()
    }
}

fn card_style(_: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(c(pal().card))),
        border: square_border(c(pal().border), 1.0),
        ..Default::default()
    }
}

/// Квадратная кнопка: без скруглений, рамка 1 px.
fn flat_button(label: &str, on_press: Option<Message>) -> Element<'_, Message> {
    let enabled = on_press.is_some();
    let b = button(text(label).size(12)).padding([6, 12]).style(
        move |_: &Theme, status: button::Status| {
            let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: Some(Background::Color(if hover && enabled {
                    c(pal().card_hi)
                } else {
                    Color::TRANSPARENT
                })),
                text_color: if enabled {
                    c(pal().text)
                } else {
                    c(pal().muted)
                },
                border: square_border(c(pal().border), 1.0),
                ..Default::default()
            }
        },
    );
    match on_press {
        Some(m) => b.on_press(m).into(),
        None => b.into(),
    }
}

/// Переключатель ВКЛ/ВЫКЛ в виде кнопки (`message` — что случится при нажатии).
fn toggle_button<'a>(on: bool, message: Message) -> Element<'a, Message> {
    chip_button(if on { "ON" } else { "OFF" }.to_string(), on, message)
}

/// Кнопка-флажок: залита акцентом, когда включена.
fn chip_button<'a>(label: String, on: bool, message: Message) -> Element<'a, Message> {
    button(text(label).size(12))
        .on_press(message)
        .padding([6, 14])
        .style(move |_: &Theme, status: button::Status| {
            let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: Some(Background::Color(if on {
                    c(pal().accent)
                } else if hover {
                    c(pal().card_hi)
                } else {
                    Color::TRANSPARENT
                })),
                text_color: if on { c(pal().bg) } else { c(pal().muted) },
                border: square_border(c(pal().border), 1.0),
                ..Default::default()
            }
        })
        .into()
}

fn nav_button<'a>(label: String, selected: bool, message: Message) -> Element<'a, Message> {
    button(text(label).size(12))
        .on_press(message)
        .padding([7, 10])
        .style(move |_: &Theme, status: button::Status| {
            let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: Some(Background::Color(if selected {
                    c(pal().card)
                } else if hover {
                    c(pal().card_hi)
                } else {
                    Color::TRANSPARENT
                })),
                text_color: if selected {
                    c(pal().accent)
                } else {
                    c(pal().text)
                },
                border: square_border(
                    if selected {
                        c(pal().accent)
                    } else {
                        Color::TRANSPARENT
                    },
                    if selected { 1.0 } else { 0.0 },
                ),
                ..Default::default()
            }
        })
        .into()
}

/// Пункт выпадающего списка «добавить в плейлист».
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlaylistChoice {
    id: String,
    name: String,
}

impl std::fmt::Display for PlaylistChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// Что показывать у строки в режиме правки.
#[derive(Clone, Copy)]
struct EditCtx {
    /// Перетаскивать можно только полный список (не результаты поиска).
    can_drag: bool,
    /// Ждём подтверждения удаления из библиотеки.
    confirm: bool,
    mark: Mark,
}

/// Ползунок с квадратной ручкой и прямоугольной дорожкой (в общей геометрии интерфейса).
fn square_slider(_: &Theme, status: slider::Status) -> slider::Style {
    let handle = match status {
        slider::Status::Active => c(pal().accent),
        slider::Status::Hovered | slider::Status::Dragged => c(pal().text),
    };
    slider::Style {
        rail: slider::Rail {
            backgrounds: (
                Background::Color(c(pal().accent)),
                Background::Color(c(pal().border)),
            ),
            width: 4.0,
            border: square_border(Color::TRANSPARENT, 0.0),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Rectangle {
                width: 16,
                border_radius: 0.0.into(),
            },
            background: Background::Color(handle),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

/// Кнопка «×» без рамки и фона (закрыть окно).
fn close_button<'a>(message: Message) -> Element<'a, Message> {
    button(text("×").size(22).color(c(pal().muted)))
        .on_press(message)
        .padding([0, 6])
        .style(|_: &Theme, status: button::Status| {
            let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: None,
                text_color: if hover { c(pal().text) } else { c(pal().muted) },
                border: square_border(Color::TRANSPARENT, 0.0),
                ..Default::default()
            }
        })
        .into()
}

/// Кнопка «+» без рамки и фона: размер и цвет как у подписи «PLAYLISTS».
fn plus_button<'a>(message: Message) -> Element<'a, Message> {
    button(text("+").size(12).color(c(pal().muted)))
        .on_press(message)
        .padding(0)
        .style(|_: &Theme, _| button::Style {
            background: None,
            text_color: c(pal().muted),
            border: square_border(Color::TRANSPARENT, 0.0),
            ..Default::default()
        })
        .into()
}

/// Парящая квадратная кнопка «[E]»: без подложки, только рамка и надпись;
/// включённая заливается акцентом.
fn edit_button<'a>(on: bool) -> Element<'a, Message> {
    button(
        container(text("[E]").size(12))
            .center_x(Length::Fill)
            .center_y(Length::Fill),
    )
    .on_press(Message::ToggleEdit)
    .width(36)
    .height(36)
    .padding(0)
    .style(move |_: &Theme, status: button::Status| {
        let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
        button::Style {
            background: Some(Background::Color(if on {
                c(pal().accent)
            } else if hover {
                c(pal().card_hi)
            } else {
                Color::TRANSPARENT
            })),
            text_color: if on {
                c(pal().bg)
            } else if hover {
                c(pal().text)
            } else {
                c(pal().muted)
            },
            border: square_border(if on { c(pal().accent) } else { c(pal().border) }, 1.0),
            ..Default::default()
        }
    })
    .into()
}

/// Ручка «=»: за неё строку тянут мышью.
fn drag_handle<'a>(pos: usize, enabled: bool) -> Element<'a, Message> {
    let label = container(text("=").size(14))
        .padding([5, 13])
        .style(move |_: &Theme| container::Style {
            text_color: Some(if enabled {
                c(pal().text)
            } else {
                c(pal().muted)
            }),
            border: square_border(c(pal().border), 1.0),
            ..Default::default()
        });
    if enabled {
        mouse_area(label)
            .on_press(Message::DragStart(pos))
            .interaction(iced::mouse::Interaction::Grab)
            .into()
    } else {
        label.into()
    }
}

/// Мышь во время перетаскивания: движение и отпускание левой кнопки.
fn on_event(
    event: iced::Event,
    _status: iced::event::Status,
    _window: window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Mouse(iced::mouse::Event::CursorMoved { position }) => {
            Some(Message::DragMove(position.y))
        }
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left)) => {
            Some(Message::DragEnd)
        }
        _ => None,
    }
}

fn track_row<'a>(
    pos: usize,
    track: &'a Track,
    active: bool,
    in_playlist: bool,
    playlists: &[PlaylistChoice],
    edit: Option<EditCtx>,
) -> Element<'a, Message> {
    let mark = edit.map_or(Mark::None, |e| e.mark);
    let title = if track.title.is_empty() {
        track.file_name()
    } else {
        track.title.clone()
    };
    let content = row![
        column![
            text(title).size(13),
            text(format!("{} · {}", track.artist, track.album))
                .size(11)
                .color(c(pal().muted)),
        ]
        .width(Length::Fill),
        text(track.duration_text()).size(12).color(c(pal().muted)),
    ]
    .align_y(Alignment::Center)
    .spacing(10);

    let main = button(content)
        .on_press(if edit.is_some() {
            Message::TagStart(track.id.clone())
        } else {
            Message::PlayAt(pos)
        })
        .width(Length::Fill)
        .height(ROW_H)
        .padding([4, 10])
        .style(move |_: &Theme, status: button::Status| {
            let hover = matches!(status, button::Status::Hovered | button::Status::Pressed);
            button::Style {
                background: Some(Background::Color(if active || mark == Mark::Dragged {
                    c(pal().card)
                } else if hover {
                    c(pal().card_hi)
                } else {
                    Color::TRANSPARENT
                })),
                text_color: if mark == Mark::Dragged {
                    c(pal().muted)
                } else if active {
                    c(pal().accent)
                } else {
                    c(pal().text)
                },
                // Куда встанет строка, если отпустить кнопку мыши.
                border: if mark == Mark::Target {
                    square_border(c(pal().accent), 1.0)
                } else {
                    square_border(Color::TRANSPARENT, 0.0)
                },
                ..Default::default()
            }
        });

    // Справа: «−» в плейлисте или список «+ в плейлист».
    let side: Element<'a, Message> = if let Some(ctx) = edit {
        let id = track.id.clone();
        let delete = if in_playlist {
            flat_button("DEL", Some(Message::RemoveFromPlaylist(id)))
        } else if ctx.confirm {
            flat_button("SURE?", Some(Message::RemoveTrack(id)))
        } else {
            flat_button("DEL", Some(Message::RemoveAsk(id)))
        };
        row![drag_handle(pos, ctx.can_drag), delete]
            .spacing(4)
            .into()
    } else if in_playlist {
        flat_button("-", Some(Message::RemoveFromPlaylist(track.id.clone())))
    } else if playlists.is_empty() {
        horizontal_space().width(0).into()
    } else {
        let track_id = track.id.clone();
        pick_list(playlists.to_vec(), None::<PlaylistChoice>, move |choice| {
            Message::AddToPlaylist {
                playlist: choice.id,
                track: track_id.clone(),
            }
        })
        .placeholder("+")
        .style(pick_list_style)
        .width(70)
        .into()
    };

    row![main, side]
        .spacing(6)
        .align_y(Alignment::Center)
        .height(ROW_H)
        .into()
}

fn run_state_label(state: RunState) -> (&'static str, Rgb) {
    match state {
        RunState::Running => ("RUNNING", pal().accent),
        RunState::Starting => ("STARTING…", pal().warn),
        RunState::Stopping => ("STOPPING", pal().warn),
        RunState::Restarting => ("RESTARTING", pal().warn),
        RunState::Failed => ("ERROR", pal().danger),
        RunState::Stopped => ("STOPPED", pal().muted),
    }
}

fn open_folder(path: &std::path::Path) {
    let program = if cfg!(windows) {
        "explorer"
    } else if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Ok(mut child) = std::process::Command::new(program).arg(path).spawn() {
        // Без wait() завершившийся процесс висел бы «зомби» до выхода из плеера.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

/// Горячие клавиши. Буквы работают и на русской раскладке.
fn on_key(key: Key, modifiers: keyboard::Modifiers) -> Option<Message> {
    // Ctrl+F — поиск. Остальные сочетания с Ctrl/Alt не трогаем: раньше Ctrl+C
    // открывал обложку, а Ctrl+S переключал перемешивание.
    if modifiers.command() || modifiers.alt() {
        return match key.as_ref() {
            Key::Character(ch) if modifiers.command() => match ch.to_lowercase().as_str() {
                "f" | "а" => Some(Message::FocusSearch),
                _ => None,
            },
            _ => None,
        };
    }
    match key.as_ref() {
        Key::Named(Named::Space) => Some(Message::Toggle),
        Key::Named(Named::ArrowLeft) => Some(Message::SeekBy(-5.0)),
        Key::Named(Named::ArrowRight) => Some(Message::SeekBy(5.0)),
        Key::Named(Named::ArrowUp) => Some(Message::VolumeBy(0.05)),
        Key::Named(Named::ArrowDown) => Some(Message::VolumeBy(-0.05)),
        Key::Named(Named::Escape) => Some(Message::Escape),
        Key::Character(ch) => match ch.to_lowercase().as_str() {
            "n" | "т" => Some(Message::Next),
            "p" | "з" => Some(Message::Prev),
            "c" | "с" => Some(Message::ToggleCover),
            "s" | "ы" => Some(Message::ToggleShuffle),
            "r" | "к" => Some(Message::CycleRepeat),
            "/" | "." => Some(Message::FocusSearch),
            _ => None,
        },
        _ => None,
    }
}

/// Картинка из модуля обложек → картинка для iced.
fn to_handle(img: crate::cover::Picture) -> Handle {
    Handle::from_rgba(img.size[0] as u32, img.size[1] as u32, img.rgba)
}
