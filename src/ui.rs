use crate::model::{self, fmt_secs, short_id, LibraryState, Playlist, Track};
use crate::player::{Player, RepeatMode};
use crate::settings::{Settings, EQ_BANDS, EQ_LABELS, EQ_MAX, EQ_MIN, PRESETS};
use crate::theme::{alpha, pal};
use crate::transfer;
use eframe::egui;
use egui::{Color32, RichText, Rounding, Stroke, Vec2};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};

#[derive(Clone, PartialEq)]
enum Tab {
    Tracks,
    Playlist(String),
    Transfer,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SettingsTab {
    Sound,
    Sync,
    Library,
    Keys,
    Look,
}

impl SettingsTab {
    fn label(self) -> &'static str {
        match self {
            SettingsTab::Sound => "ЗВУК",
            SettingsTab::Sync => "СИНХРОНИЗАЦИЯ",
            SettingsTab::Library => "БИБЛИОТЕКА",
            SettingsTab::Keys => "КЛАВИШИ",
            SettingsTab::Look => "ОФОРМЛЕНИЕ",
        }
    }
}

const SETTINGS_TABS: [SettingsTab; 5] = [
    SettingsTab::Sound,
    SettingsTab::Sync,
    SettingsTab::Library,
    SettingsTab::Keys,
    SettingsTab::Look,
];

enum Background {
    Scanned(Vec<Track>),
    Cover {
        track_id: String,
        image: egui::ColorImage,
        thumb: egui::ColorImage,
    },
    SyncProgress(String),
    SyncDone(transfer::SyncReport, String),
    Error(String),
}

pub struct App {
    state: LibraryState,
    shared: Arc<RwLock<LibraryState>>,
    player: Option<Player>,
    tab: Tab,
    search: String,
    port: u16,
    ip: String,
    server_error: Option<String>,
    rx: Receiver<Background>,
    tx: Sender<Background>,
    busy: Option<String>,
    status: String,

    new_folder: String,

    export_playlist: Option<String>,
    export_path: String,
    include_audio: bool,
    import_path: String,

    log: Vec<String>,
    rename_buffer: Option<(String, String)>,
    levels: Vec<f32>,
    cover: Option<CoverState>,
    cover_open: bool,
    cover_anim: f32,
    settings: Settings,
    settings_open: bool,
    settings_tab: SettingsTab,
    settings_dirty: bool,
    settings_saved_at: std::time::Instant,
    settings_just_opened: bool,
    /// Измеренная высота строки списка треков (для виртуализации).
    row_pitch: f32,
}

struct CoverState {
    track_id: String,
    texture: Option<egui::TextureHandle>,
    thumb: Option<egui::ColorImage>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        model::ensure_dirs();
        setup_theme(&cc.egui_ctx);

        let state = LibraryState::load();
        let shared = Arc::new(RwLock::new(state.clone()));

        let name = model::device_name();
        let mut port = 8787;
        let mut server_error = None;
        let mut started = false;
        for candidate in 8787..=8797 {
            match crate::server::spawn(shared.clone(), name.clone(), candidate) {
                Ok(()) => {
                    port = candidate;
                    started = true;
                    break;
                }
                Err(e) => server_error = Some(format!("порт {candidate}: {e}")),
            }
        }
        if !started && server_error.is_none() {
            server_error = Some("не удалось запустить LAN-сервер".into());
        }

        let ip = local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "127.0.0.1".into());

        let (tx, rx) = mpsc::channel();
        let mut app = Self {
            state,
            shared,
            player: Player::new().ok(),
            tab: Tab::Tracks,
            search: String::new(),
            port,
            ip,
            server_error,
            rx,
            tx,
            busy: None,
            status: "Готово".into(),
            new_folder: dirs::home_dir()
                .map(|h| h.join("Music").to_string_lossy().to_string())
                .unwrap_or_default(),
            export_playlist: None,
            export_path: String::new(),
            include_audio: true,
            import_path: model::imports_dir().to_string_lossy().to_string(),
            log: Vec::new(),
            rename_buffer: None,
            levels: vec![0.0; crate::vis::BANDS],
            cover: None,
            cover_open: false,
            cover_anim: 0.0,
            settings: Settings::load(),
            settings_open: false,
            settings_tab: SettingsTab::Sound,
            settings_dirty: false,
            settings_saved_at: std::time::Instant::now(),
            settings_just_opened: false,
            row_pitch: 0.0,
        };
        let volume = app.settings.volume;
        let fx = app.fx_params();
        if let Some(player) = &mut app.player {
            player.set_volume(volume);
            player.set_fx(fx);
        }
        let _ = cc;
        if !app.state.folders.is_empty() {
            app.start_scan();
        }
        app
    }

    fn fx_params(&self) -> crate::fx::FxParams {
        crate::fx::FxParams {
            enabled: self.settings.eq_enabled,
            gains: self.settings.eq_array(),
            pan: self.settings.pan,
        }
    }

    fn save_settings(&mut self) {
        if let Some(player) = &self.player {
            self.settings.volume = player.volume;
        }
        self.settings.save();
        self.settings_dirty = false;
        self.settings_saved_at = std::time::Instant::now();
    }

    fn open_settings(&mut self, tab: SettingsTab) {
        self.settings_tab = tab;
        self.settings_open = true;
        self.settings_just_opened = true;
    }

    pub fn lan_url(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }

    fn persist(&mut self) {
        self.state.normalize();
        self.state.save();
        if let Ok(mut guard) = self.shared.write() {
            *guard = self.state.clone();
        }
    }

    fn start_scan(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let folders = self.state.folders.clone();
        let known = self.state.tracks.clone();
        let tx = self.tx.clone();
        self.busy = Some("Сканирую папки…".into());
        std::thread::spawn(move || {
            let tracks = crate::library::scan(&folders, &known);
            let _ = tx.send(Background::Scanned(tracks));
        });
    }

    fn start_sync(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let peer = self.settings.peer.clone();
        if peer.trim().is_empty() {
            self.status = "Укажи адрес второго устройства (например 192.168.1.50:8787)".into();
            return;
        }
        let mine = self.state.tracks.clone();
        let tx = self.tx.clone();
        self.busy = Some("Синхронизация…".into());
        self.log.clear();
        std::thread::spawn(move || {
            let tx_progress = tx.clone();
            let result = transfer::sync_with_peer(&peer, &mine, move |msg| {
                let _ = tx_progress.send(Background::SyncProgress(msg));
            });
            match result {
                Ok(report) => {
                    let _ = tx.send(Background::SyncDone(report, peer));
                }
                Err(e) => {
                    let _ = tx.send(Background::Error(format!("Синхронизация не удалась: {e}")));
                }
            }
        });
    }

    fn play_tracks(&mut self, tracks: Vec<Track>, start: usize) {
        if let Some(player) = &mut self.player {
            player.play_queue(tracks, start);
        } else {
            self.status = "Аудиовыход недоступен".into();
        }
    }

    /// Подгружает обложку текущего трека в фоне (встроенную или заглушку).
    fn request_cover(&mut self) {
        let Some(track) = self.player.as_ref().and_then(|p| p.current().cloned()) else {
            return;
        };
        if self
            .cover
            .as_ref()
            .map(|c| c.track_id == track.id)
            .unwrap_or(false)
        {
            return;
        }
        self.cover = Some(CoverState {
            track_id: track.id.clone(),
            texture: None,
            thumb: None,
        });
        let tx = self.tx.clone();
        let id = track.id.clone();
        let path = track.path.clone();
        std::thread::spawn(move || {
            let image = crate::cover::load(&path).unwrap_or_else(crate::cover::fallback);
            let thumb = crate::cover::thumb(&image, 64);
            let _ = tx.send(Background::Cover {
                track_id: id,
                image,
                thumb,
            });
        });
    }

    fn animate_cover(&mut self, ctx: &egui::Context) {
        let target = if self.cover_open { 1.0 } else { 0.0 };
        if (self.cover_anim - target).abs() < 0.0005 {
            self.cover_anim = target;
            return;
        }
        let dt = ctx.input(|i| i.stable_dt).min(0.08);
        let speed = if self.cover_open { 1.0 / 0.85 } else { 1.0 / 0.55 };
        let step = dt * speed;
        if self.cover_anim < target {
            self.cover_anim = (self.cover_anim + step).min(1.0);
        } else {
            self.cover_anim = (self.cover_anim - step).max(0.0);
        }
        ctx.request_repaint();
    }

    fn play_playlist(&mut self, playlist_id: &str, shuffle: bool) {
        let Some(playlist) = self.state.playlist(playlist_id).cloned() else {
            return;
        };
        let mut tracks = self.state.tracks_of(&playlist);
        if shuffle {
            pseudo_shuffle(&mut tracks);
        }
        self.play_tracks(tracks, 0);
    }

    fn create_playlist(&mut self, name: &str) -> String {
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
        self.persist();
        id
    }

    fn add_to_playlist(&mut self, playlist_id: &str, track_id: &str) {
        if let Some(pl) = self.state.playlist_mut(playlist_id) {
            if !pl.track_ids.iter().any(|id| id == track_id) {
                pl.track_ids.push(track_id.to_string());
                self.persist();
                self.status = "Трек добавлен в плейлист".into();
            }
        }
    }

    fn poll_background(&mut self, ctx: &egui::Context) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Background::Scanned(mut tracks) => {
                    // Импортированные из .m3u/.zip треки лежат вне сканируемых папок
                    // (например в ~/mpt.mp/imports). Раньше первый же скан выбрасывал их
                    // из библиотеки, а normalize() потом вычищал их из плейлиста.
                    // Оставляем всё, что состоит в плейлистах и ещё есть на диске.
                    let known: std::collections::HashSet<String> =
                        tracks.iter().map(|t| t.path.clone()).collect();
                    let used: std::collections::HashSet<&String> = self
                        .state
                        .playlists
                        .iter()
                        .flat_map(|p| p.track_ids.iter())
                        .collect();
                    for old in &self.state.tracks {
                        if used.contains(&old.id)
                            && !known.contains(&old.path)
                            && std::path::Path::new(&old.path).is_file()
                        {
                            tracks.push(old.clone());
                        }
                    }
                    let count = tracks.len();
                    self.state.tracks = tracks;
                    self.persist();
                    self.busy = None;
                    self.status = format!("Библиотека: {count} треков");
                }
                Background::Cover {
                    track_id,
                    image,
                    thumb,
                } => {
                    if self
                        .cover
                        .as_ref()
                        .map(|c| c.track_id == track_id)
                        .unwrap_or(false)
                    {
                        let texture = ctx.load_texture(
                            format!("cover-{track_id}"),
                            image,
                            egui::TextureOptions::LINEAR,
                        );
                        if let Some(state) = &mut self.cover {
                            state.texture = Some(texture);
                            state.thumb = Some(thumb);
                        }
                        ctx.request_repaint();
                    }
                }
                Background::SyncProgress(line) => {
                    self.log.push(line);
                    if self.log.len() > 200 {
                        self.log.remove(0);
                    }
                    self.busy = Some("Синхронизация…".into());
                }
                Background::SyncDone(report, peer) => {
                    self.busy = None;
                    let mut summary = format!(
                        "Синхронизация с {}: получено {} ({}), отправлено {} ({})",
                        if report.peer_name.is_empty() {
                            peer
                        } else {
                            report.peer_name.clone()
                        },
                        report.pulled,
                        transfer::human_bytes(report.pulled_bytes),
                        report.pushed,
                        transfer::human_bytes(report.pushed_bytes),
                    );
                    if !report.errors.is_empty() {
                        summary.push_str(&format!(", ошибок: {}", report.errors.len()));
                        for e in &report.errors {
                            self.log.push(format!("Ошибка: {e}"));
                        }
                    }
                    self.log.push(summary.clone());
                    self.status = summary;
                    if report.pulled > 0 {
                        self.start_scan();
                    }
                }
                Background::Error(e) => {
                    self.busy = None;
                    self.status = e.clone();
                    self.log.push(e);
                }
            }
        }
        let repaint = if self.busy.is_some() {
            200
        } else if self.player.as_ref().map(|p| p.playing).unwrap_or(false) {
            200
        } else {
            600
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(repaint));
    }

    fn filtered_tracks(&self, tracks: &[Track]) -> Vec<Track> {
        let q = self.search.trim().to_lowercase();
        if q.is_empty() {
            return tracks.to_vec();
        }
        tracks
            .iter()
            .filter(|t| track_matches(t, &q))
            .cloned()
            .collect()
    }

    /// То же, что `filtered_tracks`, но забирает список целиком: без запроса
    /// возвращает его как есть, без второй копии всей библиотеки на каждый кадр.
    fn filter_owned(&self, mut tracks: Vec<Track>) -> Vec<Track> {
        let q = self.search.trim().to_lowercase();
        if !q.is_empty() {
            tracks.retain(|t| track_matches(t, &q));
        }
        tracks
    }

    fn visible_tracks(&self) -> Vec<Track> {
        match &self.tab {
            Tab::Playlist(id) => self
                .state
                .playlist(id)
                .map(|p| self.filtered_tracks(&self.state.tracks_of(p)))
                .unwrap_or_default(),
            _ => self.filtered_tracks(&self.state.tracks),
        }
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if self.settings_open {
            return;
        }
        if ctx.wants_keyboard_input() {
            return;
        }
        let (space, left, right, up, down, escape, next_key, prev_key) = ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::ArrowLeft),
                i.key_pressed(egui::Key::ArrowRight),
                i.key_pressed(egui::Key::ArrowUp),
                i.key_pressed(egui::Key::ArrowDown),
                i.key_pressed(egui::Key::Escape),
                i.key_pressed(egui::Key::N),
                i.key_pressed(egui::Key::P),
            )
        });
        if ctx.input(|i| i.key_pressed(egui::Key::S)) {
            self.open_settings(SettingsTab::Sound);
            return;
        }
        let cover_key = ctx.input(|i| i.key_pressed(egui::Key::C));
        if cover_key {
            if self.cover_open {
                self.cover_open = false;
            } else if self
                .player
                .as_ref()
                .and_then(|p| p.current().map(|_| ()))
                .is_some()
            {
                self.cover_open = true;
            }
        }
        if self.cover_open {
            if escape {
                self.cover_open = false;
            }
            if space {
                if let Some(p) = &mut self.player {
                    p.toggle();
                }
            }
            if let Some(p) = &mut self.player {
                if next_key {
                    p.next();
                }
                if prev_key {
                    p.prev();
                }
                if left {
                    p.seek((p.position - 5.0).max(0.0));
                }
                if right {
                    p.seek((p.position + 5.0).min(p.duration));
                }
                if up {
                    p.set_volume(p.volume + 0.05);
                }
                if down {
                    p.set_volume(p.volume - 0.05);
                }
            }
            return;
        }
        if escape && !self.search.is_empty() {
            self.search.clear();
        }
        let tab_key = ctx.input(|i| {
            [egui::Key::Num1, egui::Key::Num2]
                .iter()
                .position(|k| i.key_pressed(*k))
        });
        match tab_key {
            Some(0) => self.tab = Tab::Tracks,
            Some(1) => self.tab = Tab::Transfer,
            _ => {}
        }
        if space {
            let empty = self
                .player
                .as_ref()
                .map(|p| p.current().is_none())
                .unwrap_or(true);
            if empty {
                let list = self.visible_tracks();
                if !list.is_empty() {
                    self.play_tracks(list, 0);
                }
            } else if let Some(p) = &mut self.player {
                p.toggle();
            }
        }
        if let Some(p) = &mut self.player {
            if left {
                p.seek((p.position - 5.0).max(0.0));
            }
            if right {
                p.seek((p.position + 5.0).min(p.duration));
            }
            if up {
                p.set_volume(p.volume + 0.05);
                self.settings.volume = p.volume;
                self.settings_dirty = true;
                self.status = format!("Громкость: {:.0}%", p.volume * 100.0);
            }
            if down {
                p.set_volume(p.volume - 0.05);
                self.settings.volume = p.volume;
                self.settings_dirty = true;
                self.status = format!("Громкость: {:.0}%", p.volume * 100.0);
            }
            if next_key {
                p.next();
            }
            if prev_key {
                p.prev();
            }
        }
    }

    fn header(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .exact_height(56.0)
            .frame(
                egui::Frame::none()
                    .fill(pal().panel)
                    .inner_margin(egui::Margin::symmetric(18.0, 9.0)),
            )
            .show(ctx, |ui| {
                // Нижняя граница панели — как у тайлов в Omarchy.
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    ui.max_rect().bottom() + 8.0,
                    Stroke::new(1.0_f32, pal().border),
                );
                ui.horizontal_centered(|ui| {
                    let (dot_rect, _) =
                        ui.allocate_exact_size(Vec2::new(8.0, 8.0), egui::Sense::hover());
                    ui.painter().rect_filled(
                        egui::Rect::from_center_size(dot_rect.center(), Vec2::splat(7.0)),
                        Rounding::ZERO,
                        pal().accent,
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 0.0;
                        ui.label(RichText::new("mpt").font(pixel(13.0)).color(pal().text));
                        ui.label(RichText::new(".mp").font(pixel(13.0)).color(pal().accent));
                    });
                    ui.add_space(22.0);

                    let search = egui::TextEdit::singleline(&mut self.search)
                        .hint_text("/ поиск: трек, исполнитель, альбом")
                        .desired_width(320.0)
                        .margin(Vec2::new(9.0, 6.0));
                    ui.add(search);

                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            let scan_text = if self.busy.is_some() { "…" } else { "СКАНИРОВАТЬ" };
                            if ui
                                .add(
                                    egui::Button::new(
                                        RichText::new(format!("[ {scan_text} ]"))
                                            .font(mono_font(10.5))
                                            .color(pal().bg),
                                    )
                                    .fill(pal().accent)
                                    .rounding(Rounding::ZERO),
                                )
                                .clicked()
                            {
                                self.start_scan();
                            }
                            ui.add_space(8.0);
                            if ui
                                .add(
                                    egui::Button::new(mono("[ НАСТРОЙКИ ]", 10.5, pal().text))
                                        .fill(Color32::TRANSPARENT)
                                        .stroke(Stroke::new(1.0_f32, pal().border))
                                        .rounding(Rounding::ZERO),
                                )
                                .clicked()
                            {
                                self.open_settings(SettingsTab::Sound);
                            }
                            let dot = if self.server_error.is_some() {
                                pal().danger
                            } else {
                                pal().accent
                            };
                            status_dot(ui, dot);
                            ui.label(
                                RichText::new(format!("{}:{}", self.ip, self.port))
                                    .font(mono_font(10.5))
                                    .color(pal().muted),
                            );
                            ui.add_space(4.0);
                            ui.label(
                                RichText::new("LAN")
                                    .font(mono_font(10.0))
                                    .strong()
                                    .color(pal().muted),
                            );
                        },
                    );
                });
            });
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("nav")
            .exact_width(228.0)
            .resizable(false)
            .frame(
                egui::Frame::none()
                    .fill(pal().panel)
                    .inner_margin(egui::Margin::symmetric(14.0, 16.0)),
            )
            .show(ctx, |ui| {
                // Вертикальный разделитель справа, в духе рамок Omarchy.
                let rect = ui.max_rect();
                ui.painter().vline(
                    rect.right() + 13.0,
                    rect.y_range(),
                    Stroke::new(1.0_f32, pal().border),
                );
                ui.add_space(4.0);
                section_title(ui, "БИБЛИОТЕКА");
                let tracks_count = self.state.tracks.len();
                if nav_item(ui, "Все треки", &format!("{tracks_count}"), self.tab == Tab::Tracks).clicked() {
                    self.tab = Tab::Tracks;
                }

                ui.add_space(16.0);
                section_title(ui, "ПЛЕЙЛИСТЫ");
                let playlists = self.state.playlists.clone();
                for pl in &playlists {
                    let current = self.tab == Tab::Playlist(pl.id.clone());
                    let resp = nav_item(
                        ui,
                        &pl.name,
                        &pl.track_ids.len().to_string(),
                        current,
                    );
                    if resp.clicked() {
                        self.tab = Tab::Playlist(pl.id.clone());
                    }
                    resp.context_menu(|ui| {
                        if ui.button("Переименовать").clicked() {
                            self.rename_buffer = Some((pl.id.clone(), pl.name.clone()));
                            ui.close_menu();
                        }
                        if ui.button("Экспорт .m3u").clicked() {
                            self.export_playlist = Some(pl.id.clone());
                            self.tab = Tab::Transfer;
                            ui.close_menu();
                        }
                        if ui.button(RichText::new("Удалить").color(pal().danger)).clicked() {
                            self.state.playlists.retain(|p| p.id != pl.id);
                            if self.tab == Tab::Playlist(pl.id.clone()) {
                                self.tab = Tab::Tracks;
                            }
                            self.persist();
                            ui.close_menu();
                        }
                    });
                }
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("+  новый плейлист")
                                .font(mono_font(11.0))
                                .color(pal().accent),
                        )
                        .frame(false),
                    )
                    .clicked()
                {
                    let name = format!("Плейлист {}", self.state.playlists.len() + 1);
                    let id = self.create_playlist(&name);
                    self.tab = Tab::Playlist(id);
                }

                ui.add_space(16.0);
                section_title(ui, "ИНСТРУМЕНТЫ");
                if nav_item(ui, "Экспорт / Импорт", "", self.tab == Tab::Transfer).clicked() {
                    self.tab = Tab::Transfer;
                }
                if nav_item(ui, "Настройки", "S", false).clicked() {
                    self.open_settings(SettingsTab::Sound);
                }

                ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
                    ui.label(
                        RichText::new("mpt.mp 0.1")
                            .font(mono_font(10.0))
                            .color(pal().muted),
                    );
                    ui.label(
                        RichText::new("S — настройки")
                            .font(mono_font(9.5))
                            .color(pal().muted),
                    );
                    ui.label(
                        RichText::new("C — обложка")
                            .font(mono_font(9.5))
                            .color(pal().muted),
                    );
                });
            });
    }

    fn player_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("player")
            .exact_height(98.0)
            .frame(
                egui::Frame::none()
                    .fill(pal().panel)
                    .inner_margin(egui::Margin::symmetric(18.0, 8.0)),
            )
            .show(ctx, |ui| {
                // Верхняя граница панели.
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    ui.max_rect().top() - 8.5,
                    Stroke::new(1.0_f32, pal().border),
                );
                let (mut pos, mut dur, mut playing, mut volume, mut repeat, mut shuffle) =
                    (0.0f32, 0.0f32, false, 0.8f32, RepeatMode::Off, false);
                let mut current: Option<Track> = None;
                if let Some(p) = &self.player {
                    pos = p.position;
                    dur = p.duration.max(0.1);
                    playing = p.playing;
                    volume = p.volume;
                    repeat = p.repeat;
                    shuffle = p.shuffle;
                    current = p.current().cloned();
                }

                let mut seek_to: Option<f32> = None;
                ui.horizontal(|ui| {
                    ui.label(mono(fmt_secs(pos as u64), 10.5, pal().muted));
                    ui.spacing_mut().slider_width = (ui.available_width() - 64.0).max(120.0);
                    let resp = ui.add(
                        egui::Slider::new(&mut pos, 0.0..=dur)
                            .show_value(false)
                            .trailing_fill(true),
                    );
                    if resp.drag_stopped() || (resp.clicked() && !resp.dragged()) {
                        seek_to = Some(pos);
                    }
                    ui.label(mono(fmt_secs(dur as u64), 10.5, pal().muted));
                });

                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    let (rect, cover_resp) =
                        ui.allocate_exact_size(Vec2::splat(46.0), egui::Sense::click());
                    let cover_resp = cover_resp
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .on_hover_text("Открыть обложку");
                    let painter = ui.painter();
                    painter.rect_filled(rect, Rounding::ZERO, pal().card);
                    if let Some(tex) = self.cover.as_ref().and_then(|c| c.texture.as_ref()) {
                        painter.image(
                            tex.id(),
                            rect,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    } else {
                        painter.text(
                            rect.center(),
                            egui::Align2::CENTER_CENTER,
                            "♪",
                            pixel(12.0),
                            pal().accent,
                        );
                    }
                    let border = if cover_resp.hovered() {
                        pal().accent
                    } else {
                        pal().border
                    };
                    painter.rect_stroke(rect, Rounding::ZERO, Stroke::new(1.0_f32, border));
                    if cover_resp.clicked() {
                        self.cover_open = true;
                    }

                    ui.add_space(12.0);
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        match &current {
                            Some(t) => {
                                ui.label(
                                    RichText::new(&t.title)
                                        .font(mono_font(13.0))
                                        .strong()
                                        .color(pal().text),
                                );
                                ui.label(mono(track_subtitle(t), 10.5, pal().muted));
                            }
                            None => {
                                ui.label(
                                    RichText::new("ничего не играет")
                                        .font(mono_font(13.0))
                                        .color(pal().text),
                                );
                                ui.label(mono("выбери трек из библиотеки", 10.5, pal().muted));
                            }
                        }
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let vol_resp = ui
                            .add(
                                egui::Button::new(mono(volume_label(volume), 9.5, pal().muted))
                                    .frame(false)
                                    .rounding(Rounding::ZERO),
                            )
                            .on_hover_text("Настройки звука (S)")
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        if vol_resp.clicked() {
                            self.open_settings(SettingsTab::Sound);
                        }
                        let (icon_rect, icon_resp) =
                            ui.allocate_exact_size(Vec2::new(18.0, 18.0), egui::Sense::click());
                        let icon_resp = icon_resp
                            .on_hover_text("Настройки звука")
                            .on_hover_cursor(egui::CursorIcon::PointingHand);
                        paint_volume_icon(ui.painter(), icon_rect, pal().muted);
                        if icon_resp.clicked() {
                            self.open_settings(SettingsTab::Sound);
                        }

                        ui.add_space(16.0);
                        if transport_button(ui, IconKind::Next, 32.0, None, pal().text).clicked() {
                            if let Some(p) = &mut self.player {
                                p.next();
                            }
                        }

                        let kind = if playing { IconKind::Pause } else { IconKind::Play };
                        if transport_button(ui, kind, 36.0, Some(pal().accent), pal().bg).clicked() {
                            if let Some(p) = &mut self.player {
                                p.toggle();
                            }
                        }

                        if transport_button(ui, IconKind::Prev, 32.0, None, pal().text).clicked() {
                            if let Some(p) = &mut self.player {
                                p.prev();
                            }
                        }

                        ui.add_space(12.0);
                        let rep_label = match repeat {
                            RepeatMode::Off => "ПОВТОР",
                            RepeatMode::All => "ПОВТОР: ВСЁ",
                            RepeatMode::One => "ПОВТОР: 1",
                        };
                        let rep_color = if repeat == RepeatMode::Off { pal().muted } else { pal().accent };
                        if ui
                            .add(
                                egui::Button::new(mono(rep_label, 9.5, rep_color))
                                    .frame(false)
                                    .rounding(Rounding::ZERO),
                            )
                            .clicked()
                        {
                            if let Some(p) = &mut self.player {
                                p.repeat = p.repeat.next();
                            }
                        }
                        let sh_color = if shuffle { pal().accent } else { pal().muted };
                        if ui
                            .add(
                                egui::Button::new(mono("ПЕРЕМЕШАТЬ", 9.5, sh_color))
                                    .frame(false)
                                    .rounding(Rounding::ZERO),
                            )
                            .clicked()
                        {
                            if let Some(p) = &mut self.player {
                                p.shuffle = !p.shuffle;
                            }
                        }
                    });
                });

                if let Some(target) = seek_to {
                    if let Some(p) = &mut self.player {
                        p.seek(target);
                    }
                }
            });
    }

    fn status_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("status")
            .exact_height(26.0)
            .frame(
                egui::Frame::none()
                    .fill(pal().bg)
                    .inner_margin(egui::Margin::symmetric(18.0, 3.0)),
            )
            .show(ctx, |ui| {
                ui.painter().hline(
                    ui.max_rect().x_range(),
                    ui.max_rect().top() - 1.0,
                    Stroke::new(1.0_f32, alpha(pal().border, 160)),
                );
                ui.horizontal(|ui| {
                    let msg = self.busy.clone().unwrap_or_else(|| self.status.clone());
                    ui.label(mono(msg, 10.0, pal().muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.server_error.is_some() {
                            ui.label(mono("сервер: ошибка", 10.0, pal().danger));
                        } else {
                            ui.label(mono("сервер: работает", 10.0, pal().muted));
                        }
                        ui.add_space(12.0);
                        ui.label(mono(
                            format!("папок: {}", self.state.folders.len()),
                            10.0,
                            pal().muted,
                        ));
                    });
                });
            });
    }

    fn rename_dialog(&mut self, ctx: &egui::Context) {
        let Some((id, buf)) = self.rename_buffer.clone() else {
            return;
        };
        let mut buffer = buf;
        let mut save = false;
        let mut close = false;
        egui::Window::new("Переименовать плейлист")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .frame(
                egui::Frame::none()
                    .fill(pal().panel)
                    .stroke(Stroke::new(1.0_f32, pal().border))
                    .rounding(Rounding::ZERO)
                    .inner_margin(egui::Margin::same(18.0)),
            )
            .show(ctx, |ui| {
                ui.label(mono("НОВОЕ ИМЯ", 9.5, pal().muted));
                ui.add_space(6.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut buffer)
                        .desired_width(280.0)
                        .margin(Vec2::new(8.0, 6.0)),
                );
                resp.request_focus();
                if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    save = true;
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::Button::new(mono("СОХРАНИТЬ", 10.0, pal().bg)).fill(pal().accent).rounding(Rounding::ZERO))
                        .clicked()
                    {
                        save = true;
                    }
                    if ui
                        .add(
                            egui::Button::new(mono("ОТМЕНА", 10.0, pal().text))
                                .fill(Color32::TRANSPARENT)
                                .stroke(Stroke::new(1.0_f32, pal().border))
                                .rounding(Rounding::ZERO),
                        )
                        .clicked()
                    {
                        close = true;
                    }
                });
            });
        if save {
            let name = buffer.trim().to_string();
            if !name.is_empty() {
                if let Some(pl) = self.state.playlist_mut(&id) {
                    pl.name = name;
                }
                self.persist();
            }
            close = true;
        }
        if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.rename_buffer = None;
        } else {
            self.rename_buffer = Some((id, buffer));
        }
    }

    fn update_levels(&mut self, ctx: &egui::Context) {
        if self.levels.len() != crate::vis::BANDS {
            self.levels = vec![0.0; crate::vis::BANDS];
        }
        let playing = self
            .player
            .as_ref()
            .map(|p| p.playing && p.current().is_some())
            .unwrap_or(false);

        // Визуализатор рисуется только на страницах библиотеки и только если включён.
        // Иначе FFT и перерисовка 30 раз в секунду впустую грузят процессор.
        let visible = self.settings.visualizer
            && matches!(self.tab, Tab::Tracks | Tab::Playlist(_));
        if !visible {
            for level in &mut self.levels {
                *level = 0.0;
            }
            return;
        }

        if playing {
            let mut samples = Vec::with_capacity(crate::vis::FFT_N);
            if let Some(player) = &self.player {
                player.copy_spectrum(&mut samples);
            }
            let target = crate::vis::bands(&samples);
            for (level, peak) in self.levels.iter_mut().zip(target) {
                let k = if peak > *level { 0.55 } else { 0.10 };
                *level += (peak - *level) * k;
            }
        } else {
            for level in &mut self.levels {
                *level *= 0.82;
            }
        }

        let alive = self.levels.iter().any(|v| *v > 0.004);
        if playing || alive {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
    }

    fn central(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(pal().bg).inner_margin(egui::Margin::symmetric(20.0, 16.0)))
            .show(ctx, |ui| {
                let tab = self.tab.clone();
                let library = matches!(tab, Tab::Tracks | Tab::Playlist(_));
                if library && self.settings.visualizer {
                    paint_visualizer(ui, &self.levels);
                }
                match tab {
                    Tab::Tracks => self.view_tracks(ui, None),
                    Tab::Playlist(id) => self.view_tracks(ui, Some(id)),
                    Tab::Transfer => {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| self.view_transfer(ui));
                    }
                }
            });
    }

    /// Полноэкранная обложка с пиксельной анимацией сборки/распада.
    fn cover_overlay(&mut self, ctx: &egui::Context) {
        let progress = self.cover_anim;
        if progress <= 0.0005 {
            return;
        }
        let screen = ctx.screen_rect();
        let mut close = false;

        let track = self.player.as_ref().and_then(|p| p.current().cloned());
        let (pos, dur, playing) = self
            .player
            .as_ref()
            .map(|p| (p.position, p.duration.max(0.1), p.playing))
            .unwrap_or((0.0, 0.1, false));

        egui::Area::new(egui::Id::new("cover_overlay"))
            .fixed_pos(screen.min)
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                let (rect, resp) = ui.allocate_exact_size(screen.size(), egui::Sense::click());
                let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    close = true;
                }

                let painter = ui.painter().clone();
                let dim = smoothstep(0.0, 0.32, progress);
                painter.rect_filled(
                    rect,
                    Rounding::ZERO,
                    Color32::from_black_alpha((dim * 210.0) as u8),
                );

                self.paint_cover_pixels(&painter, rect, progress);

                // Готовая текстура проявляется поверх «пикселей».
                let tex_alpha = smoothstep(0.84, 0.99, progress);
                if tex_alpha > 0.0 {
                    // Подложка: пока текстура декодируется, не оставляем «дыру».
                    painter.rect_filled(
                        rect,
                        Rounding::ZERO,
                        alpha(pal().panel, (tex_alpha * 235.0) as u8),
                    );
                    if let Some(tex) = self.cover.as_ref().and_then(|c| c.texture.as_ref()) {
                        let uv = crate::cover::cover_uv(tex.size(), rect.size());
                        painter.image(
                            tex.id(),
                            rect,
                            uv,
                            Color32::from_white_alpha((tex_alpha * 255.0) as u8),
                        );
                    }
                }

                let chrome = smoothstep(0.55, 0.96, progress);
                if chrome <= 0.002 {
                    return;
                }
                let a = (chrome * 255.0) as u8;
                let margin = 40.0;

                paint_vertical_gradient(
                    &painter,
                    egui::Rect::from_min_max(
                        rect.min,
                        egui::pos2(rect.right(), rect.top() + 150.0),
                    ),
                    Color32::from_black_alpha((172.0 * chrome) as u8),
                    Color32::TRANSPARENT,
                );
                paint_vertical_gradient(
                    &painter,
                    egui::Rect::from_min_max(
                        egui::pos2(rect.left(), rect.bottom() - rect.height() * 0.46),
                        rect.max,
                    ),
                    Color32::TRANSPARENT,
                    Color32::from_black_alpha((233.0 * chrome) as u8),
                );
                // Дополнительное затемнение под текстовым блоком.
                paint_vertical_gradient(
                    &painter,
                    egui::Rect::from_min_max(
                        egui::pos2(rect.left(), rect.bottom() - 170.0),
                        rect.max,
                    ),
                    Color32::TRANSPARENT,
                    Color32::from_black_alpha((132.0 * chrome) as u8),
                );

                // Уголки-скобки в духе Omarchy.
                let bracket = alpha(pal().accent, (210.0 * chrome) as u8);
                let len = 26.0;
                let inset = 18.0;
                let stroke = Stroke::new(2.0_f32, bracket);
                for (corner, dx, dy) in [
                    (rect.left_top() + Vec2::splat(inset), 1.0, 1.0),
                    (rect.right_top() + Vec2::new(-inset, inset), -1.0, 1.0),
                    (rect.left_bottom() + Vec2::new(inset, -inset), 1.0, -1.0),
                    (rect.right_bottom() - Vec2::splat(inset), -1.0, -1.0),
                ] {
                    if dx > 0.0 {
                        painter.hline(egui::Rangef::new(corner.x, corner.x + len), corner.y, stroke);
                    } else {
                        painter.hline(egui::Rangef::new(corner.x - len, corner.x), corner.y, stroke);
                    }
                    if dy > 0.0 {
                        painter.vline(corner.x, egui::Rangef::new(corner.y, corner.y + len), stroke);
                    } else {
                        painter.vline(corner.x, egui::Rangef::new(corner.y - len, corner.y), stroke);
                    }
                }

                // Логотип и подсказка.
                let logo_y = rect.top() + 40.0;
                let logo_x = rect.left() + margin + 4.0;
                painter.text(
                    egui::pos2(logo_x, logo_y),
                    egui::Align2::LEFT_CENTER,
                    "mpt",
                    pixel(10.0),
                    Color32::from_white_alpha(a),
                );
                painter.text(
                    egui::pos2(logo_x + 31.0, logo_y),
                    egui::Align2::LEFT_CENTER,
                    ".mp",
                    pixel(10.0),
                    alpha(pal().accent, a),
                );
                painter.text(
                    egui::pos2(rect.right() - margin, logo_y),
                    egui::Align2::RIGHT_CENTER,
                    "ESC / КЛИК — ЗАКРЫТЬ",
                    mono_font(9.0),
                    Color32::from_white_alpha((a as f32 * 0.72) as u8),
                );

                // Нижний блок: состояние, название, прогресс.
                let x = rect.left() + margin + 4.0;
                let right = rect.right() - margin;
                let line_y = rect.bottom() - 40.0;

                let label_color = if playing {
                    alpha(pal().accent, a)
                } else {
                    Color32::from_white_alpha((a as f32 * 0.7) as u8)
                };
                painter.text(
                    egui::pos2(x, line_y - 68.0),
                    egui::Align2::LEFT_BOTTOM,
                    if playing { "СЕЙЧАС ИГРАЕТ" } else { "ПАУЗА" },
                    mono_font(9.0),
                    label_color,
                );

                match &track {
                    Some(t) => {
                        let title = fit_text(&t.title, 25.0, (right - x) * 0.76);
                        let shadow = Color32::from_black_alpha((a as f32 * 0.55) as u8);
                        painter.text(
                            egui::pos2(x + 1.5, line_y - 38.5),
                            egui::Align2::LEFT_BOTTOM,
                            &title,
                            mono_font(25.0),
                            shadow,
                        );
                        painter.text(
                            egui::pos2(x, line_y - 40.0),
                            egui::Align2::LEFT_BOTTOM,
                            title,
                            mono_font(25.0),
                            Color32::from_white_alpha(a),
                        );
                        let sub = fit_text(&track_subtitle(t), 11.0, right - x);
                        painter.text(
                            egui::pos2(x + 1.0, line_y - 19.0),
                            egui::Align2::LEFT_BOTTOM,
                            &sub,
                            mono_font(11.0),
                            Color32::from_black_alpha((a as f32 * 0.5) as u8),
                        );
                        painter.text(
                            egui::pos2(x, line_y - 20.0),
                            egui::Align2::LEFT_BOTTOM,
                            sub,
                            mono_font(11.0),
                            Color32::from_white_alpha((a as f32 * 0.78) as u8),
                        );
                    }
                    None => {
                        painter.text(
                            egui::pos2(x, line_y - 40.0),
                            egui::Align2::LEFT_BOTTOM,
                            "ничего не играет",
                            mono_font(20.0),
                            Color32::from_white_alpha(a),
                        );
                    }
                }

                painter.text(
                    egui::pos2(x, line_y - 6.0),
                    egui::Align2::LEFT_BOTTOM,
                    fmt_secs(pos as u64),
                    mono_font(10.0),
                    Color32::from_white_alpha((a as f32 * 0.65) as u8),
                );
                painter.text(
                    egui::pos2(right, line_y - 6.0),
                    egui::Align2::RIGHT_BOTTOM,
                    fmt_secs(dur as u64),
                    mono_font(10.0),
                    Color32::from_white_alpha((a as f32 * 0.65) as u8),
                );

                let bar = egui::Rect::from_min_max(
                    egui::pos2(x, line_y),
                    egui::pos2(right, line_y + 2.0),
                );
                painter.rect_filled(
                    bar,
                    Rounding::ZERO,
                    Color32::from_white_alpha((a as f32 * 0.22) as u8),
                );
                let ratio = (pos / dur).clamp(0.0, 1.0);
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        bar.min,
                        egui::pos2(bar.left() + bar.width() * ratio, bar.bottom()),
                    ),
                    Rounding::ZERO,
                    alpha(pal().accent, a),
                );
                let head_x = bar.left() + bar.width() * ratio;
                painter.rect_filled(
                    egui::Rect::from_min_max(
                        egui::pos2(head_x - 1.0, bar.top() - 3.0),
                        egui::pos2(head_x + 1.0, bar.bottom() + 3.0),
                    ),
                    Rounding::ZERO,
                    Color32::from_white_alpha(a),
                );
            });

        if close {
            self.cover_open = false;
        }
    }

    /// Пиксельная сборка обложки: сетка ячеек слетается из случайных точек.
    fn paint_cover_pixels(
        &self,
        painter: &egui::Painter,
        rect: egui::Rect,
        progress: f32,
    ) {
        let thumb = self.cover.as_ref().and_then(|c| c.thumb.as_ref());
        let texture = self.cover.as_ref().and_then(|c| c.texture.as_ref());
        let uv = texture
            .map(|t| crate::cover::cover_uv(t.size(), rect.size()))
            .unwrap_or_else(|| {
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
            });

        let cols = 46usize;
        let rows =
            ((cols as f32 * rect.height() / rect.width()).round() as usize).clamp(12, 80);
        let cw = rect.width() / cols as f32;
        let ch = rect.height() / rows as f32;
        let tile_fade = 1.0 - smoothstep(0.86, 0.995, progress);

        for j in 0..rows {
            for i in 0..cols {
                let cell = egui::Rect::from_min_size(
                    egui::pos2(rect.left() + i as f32 * cw, rect.top() + j as f32 * ch),
                    egui::vec2(cw, ch),
                );
                let mut h = (i as u32).wrapping_mul(0x9E37_79B1)
                    ^ (j as u32).wrapping_mul(0x85EB_CA77)
                    ^ 0xC2B2_AE35;
                let mut rnd = move || {
                    h ^= h << 13;
                    h ^= h >> 17;
                    h ^= h << 5;
                    (h & 0xFFFF) as f32 / 65535.0
                };
                let r1 = rnd();
                let r2 = rnd();
                let r3 = rnd();
                let r4 = rnd();

                let delay = r1 * 0.42;
                let local =
                    ((progress - delay) / (1.0 - delay).max(0.01)).clamp(0.0, 1.0);
                let e = (1.0 - (1.0 - local).powi(3)).clamp(0.0, 1.0);

                let start = egui::pos2(
                    rect.left() + r2 * rect.width(),
                    rect.top() + r3 * rect.height(),
                );
                let center = start.lerp(cell.center(), e);
                let scale = 0.55 + 0.45 * e;
                let size = egui::vec2(cw, ch) * scale * (1.0 + (1.0 - e) * r4 * 0.8);

                let target = thumb
                    .map(|t| {
                        sample_thumb(
                            t,
                            uv.min.x + uv.width() * (i as f32 + 0.5) / cols as f32,
                            uv.min.y + uv.height() * (j as f32 + 0.5) / rows as f32,
                        )
                    })
                    .unwrap_or(pal().card);
                let base = if r4 > 0.955 {
                    pal().accent
                } else if r4 > 0.78 {
                    pal().card_hi
                } else {
                    crate::cover::mix(pal().bg, pal().panel, r2)
                };
                let color = crate::cover::mix(base, target, e);
                let a = (255.0 * (0.65 + 0.35 * e) * tile_fade).clamp(0.0, 255.0) as u8;
                if a > 0 {
                    painter.rect_filled(
                        egui::Rect::from_center_size(center, size),
                        Rounding::ZERO,
                        Color32::from_rgba_unmultiplied(
                            color.r(),
                            color.g(),
                            color.b(),
                            a,
                        ),
                    );
                }
            }
        }
    }

    fn view_tracks(&mut self, ui: &mut egui::Ui, playlist_id: Option<String>) {
        let (title, subtitle, tracks) = match &playlist_id {
            Some(id) => {
                let Some(pl) = self.state.playlist(id).cloned() else {
                    self.tab = Tab::Tracks;
                    return;
                };
                (
                    pl.name.clone(),
                    format!(
                        "{} треков · {}",
                        pl.track_ids.len(),
                        transfer::total_duration_text(&self.state.tracks_of(&pl))
                    ),
                    self.state.tracks_of(&pl),
                )
            }
            None => (
                "Все треки".to_string(),
                format!(
                    "{} треков · {}",
                    self.state.tracks.len(),
                    transfer::total_duration_text(&self.state.tracks)
                ),
                self.state.tracks.clone(),
            ),
        };

        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                page_title(ui, &title);
                ui.label(mono(&subtitle, 10.5, pal().muted));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(mono("ПЕРЕМЕШАТЬ", 10.0, pal().text))
                            .fill(Color32::TRANSPARENT)
                            .stroke(Stroke::new(1.0_f32, pal().border))
                            .rounding(Rounding::ZERO),
                    )
                    .clicked()
                {
                    match &playlist_id {
                        Some(id) => self.play_playlist(id, true),
                        None => {
                            let mut list = self.filtered_tracks(&self.state.tracks.clone());
                            pseudo_shuffle(&mut list);
                            self.play_tracks(list, 0);
                        }
                    }
                }
                if ui
                    .add(
                        egui::Button::new(mono("▶  ИГРАТЬ ВСЁ", 10.0, pal().bg))
                            .fill(pal().accent)
                            .rounding(Rounding::ZERO),
                    )
                    .clicked()
                {
                    let list = self.filtered_tracks(&tracks);
                    self.play_tracks(list, 0);
                }
                if let Some(id) = &playlist_id {
                    if ui
                        .add(
                            egui::Button::new(mono("ЭКСПОРТ", 10.0, pal().text))
                                .fill(Color32::TRANSPARENT)
                                .stroke(Stroke::new(1.0_f32, pal().border))
                                .rounding(Rounding::ZERO),
                        )
                        .clicked()
                    {
                        self.export_playlist = Some(id.clone());
                        self.tab = Tab::Transfer;
                    }
                }
            });
        });

        if playlist_id.is_none() && self.settings.sync_enabled {
            ui.add_space(10.0);
            self.sync_strip(ui);
        }

        ui.add_space(12.0);
        let filtered = self.filter_owned(tracks);
        if filtered.is_empty() {
            empty_state(ui, &self.state);
            return;
        }

        let mut queue_request: Option<(Vec<Track>, usize)> = None;
        let mut add_request: Option<String> = None;
        let mut remove_request: Option<String> = None;
        let current_id = self.player.as_ref().and_then(|p| p.current().map(|t| t.id.clone()));
        let playlists = self.state.playlists.clone();
        let in_playlist = playlist_id.is_some();

        // Рисуем только видимые строки. Раньше на каждый кадр строился весь список:
        // на тысячах треков это главная нагрузка на процессор. Высота строки
        // измеряется по первой нарисованной (до этого — оценка).
        let total = filtered.len();
        let pitch = if self.row_pitch > 1.0 { self.row_pitch } else { 56.0 };
        let mut measured = self.row_pitch;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                let sy = ui.spacing().item_spacing.y;
                ui.add_space(2.0);
                let first = ((viewport.min.y / pitch).floor().max(0.0) as usize)
                    .saturating_sub(2)
                    .min(total);
                let last = (((viewport.max.y / pitch).ceil().max(0.0) as usize) + 3).min(total);
                if first > 0 {
                    ui.add_space((first as f32 * pitch - sy).max(0.0));
                }
                for i in first..last {
                    let track = &filtered[i];
                    let is_current = current_id.as_deref() == Some(track.id.as_str());
                    // Полупрозрачные строки: сквозь них виден визуализатор.
                    let row_fill = if is_current {
                        alpha(pal().accent, 30)
                    } else if i % 2 == 0 {
                        alpha(pal().card, 150)
                    } else {
                        alpha(pal().bg, 120)
                    };
                    let inner = egui::Frame::none()
                        .fill(row_fill)
                        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.set_min_height(30.0);
                                ui.label(mono(format!("{:02}", i + 1), 10.0, pal().muted));
                                ui.add_space(6.0);
                                ui.vertical(|ui| {
                                    ui.spacing_mut().item_spacing.y = 1.0;
                                    let color = if is_current { pal().accent } else { pal().text };
                                    let resp = ui.add(
                                        egui::Label::new(
                                            RichText::new(&track.title)
                                                .font(mono_font(12.5))
                                                .color(color),
                                        )
                                        .sense(egui::Sense::click()),
                                    );
                                    if resp.double_clicked() {
                                        queue_request = Some((filtered.clone(), i));
                                    }
                                    ui.label(mono(track_subtitle(track), 10.0, pal().muted));
                                });

                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if in_playlist {
                                        if ui
                                            .add(egui::Button::new(mono("−", 12.0, pal().muted)).frame(false))
                                            .on_hover_text("Убрать из плейлиста")
                                            .clicked()
                                        {
                                            remove_request = Some(track.id.clone());
                                        }
                                    }
                                    if !playlists.is_empty() {
                                        // Обычный клик открывает меню. Раньше меню открывалось только
                                        // правым кликом, а левый в «Все треки» ничего не делал.
                                        ui.menu_button(mono("+", 12.0, pal().muted), |ui| {
                                            ui.label(mono("Добавить в плейлист", 10.0, pal().muted));
                                            for pl in &playlists {
                                                if ui.button(&pl.name).clicked() {
                                                    add_request = Some(format!("{}|{}", pl.id, track.id));
                                                    ui.close_menu();
                                                }
                                            }
                                        });
                                    }
                                    ui.add_space(8.0);
                                    ui.label(mono(track.duration_text(), 10.0, pal().muted));
                                    ui.add_space(8.0);
                                    if ui
                                        .add(egui::Button::new(mono("▶", 11.0, pal().muted)).frame(false))
                                        .clicked()
                                    {
                                        queue_request = Some((filtered.clone(), i));
                                    }
                                });
                            });
                        });
                    if is_current {
                        let rect = inner.response.rect;
                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())),
                            Rounding::ZERO,
                            pal().accent,
                        );
                    }
                    // Шаг строки = высота рамки + отступы до и после + 1 px.
                    measured = inner.response.rect.height() + 2.0 * sy + 1.0;
                    ui.add_space(1.0);
                }
                if last < total {
                    ui.add_space(((total - last) as f32 * pitch - sy).max(0.0));
                }
            });
        self.row_pitch = measured;

        if let Some((queue, index)) = queue_request {
            self.play_tracks(queue, index);
        }
        if let Some(pair) = add_request {
            if let Some((pl_id, track_id)) = pair.split_once('|') {
                self.add_to_playlist(pl_id, track_id);
            }
        }
        if let (Some(track_id), Some(pl_id)) = (remove_request, playlist_id.clone()) {
            if let Some(pl) = self.state.playlist_mut(&pl_id) {
                pl.track_ids.retain(|id| id != &track_id);
                self.persist();
            }
        }
    }

    fn view_transfer(&mut self, ui: &mut egui::Ui) {
        page_title(ui, "Плейлисты: экспорт и импорт");
        ui.label(
            mono("m3u хранит ссылки на файлы, zip может нести сами треки", 10.5, pal().muted),
        );
        ui.add_space(12.0);

        let playlists = self.state.playlists.clone();
        if self.export_playlist.is_none() {
            self.export_playlist = playlists.first().map(|p| p.id.clone());
        }

        let avail = ui.available_width();
        if avail >= 980.0 {
            ui.horizontal_top(|ui| {
                self.export_card(ui, 460.0);
                ui.add_space(8.0);
                self.import_card(ui, 460.0);
            });
        } else {
            self.export_card(ui, avail);
            ui.add_space(10.0);
            self.import_card(ui, avail);
        }
    }

    fn export_card(&mut self, ui: &mut egui::Ui, width: f32) {
        let playlists = self.state.playlists.clone();
        card(ui, width, |ui| {
            ui.label(RichText::new("ЭКСПОРТ").font(mono_font(9.5)).strong().color(pal().accent));
            ui.add_space(8.0);

            let selected_name = self
                .export_playlist
                .as_ref()
                .and_then(|id| self.state.playlist(id))
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "— нет плейлистов —".into());
            ui.label(mono("ПЛЕЙЛИСТ", 9.5, pal().muted));
            egui::ComboBox::from_id_salt("export_playlist")
                .selected_text(selected_name)
                .width(ui.available_width() - 8.0)
                .show_ui(ui, |ui| {
                    for pl in &playlists {
                        let selected = self.export_playlist.as_deref() == Some(pl.id.as_str());
                        if ui.selectable_label(selected, &pl.name).clicked() {
                            self.export_playlist = Some(pl.id.clone());
                            self.export_path = default_export_path(&pl.name, true);
                        }
                    }
            });

            if self.export_path.is_empty() {
                if let Some(pl) = self.export_playlist.as_ref().and_then(|id| self.state.playlist(id)) {
                    self.export_path = default_export_path(&pl.name, true);
                }
            }
            ui.add_space(6.0);
            ui.label(mono("ФАЙЛ", 9.5, pal().muted));
            ui.add(egui::TextEdit::singleline(&mut self.export_path).desired_width(f32::INFINITY));
            ui.checkbox(&mut self.include_audio, "Включить аудиофайлы в ZIP");

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let enabled = self.export_playlist.is_some();
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(mono("ЭКСПОРТ .M3U", 10.0, pal().text)).fill(Color32::TRANSPARENT).stroke(Stroke::new(1.0_f32, pal().border)).rounding(Rounding::ZERO),
                    )
                    .clicked()
                {
                    self.do_export(false);
                }
                if ui
                    .add_enabled(
                        enabled,
                        egui::Button::new(mono("ЭКСПОРТ .ZIP", 10.0, pal().bg)).fill(pal().accent).rounding(Rounding::ZERO),
                    )
                    .clicked()
                {
                    self.do_export(true);
                }
            });
            ui.add_space(6.0);
            ui.label(mono(
                format!("папка: {}", model::exports_dir().display()),
                9.5,
                pal().muted,
            ));
        });
    }

    fn import_card(&mut self, ui: &mut egui::Ui, width: f32) {
        card(ui, width, |ui| {
            ui.label(RichText::new("ИМПОРТ").font(mono_font(9.5)).strong().color(pal().accent2));
            ui.add_space(8.0);
            ui.label(mono("ПУТЬ К .M3U / .M3U8 / .ZIP", 9.5, pal().muted));
            ui.add(egui::TextEdit::singleline(&mut self.import_path).desired_width(f32::INFINITY));
            ui.add_space(8.0);
            if ui
                .add(egui::Button::new(mono("ИМПОРТИРОВАТЬ", 10.0, pal().bg)).fill(pal().accent2).rounding(Rounding::ZERO))
                .clicked()
            {
                self.do_import();
            }
            ui.add_space(10.0);
            ui.label(mono(
                "импорт создаёт новый плейлист и добавляет недостающие треки",
                10.0,
                pal().muted,
            ));
            ui.add_space(6.0);
            if ui
                .add(
                    egui::Button::new(mono("ОТКРЫТЬ ПАПКУ EXPORTS", 10.0, pal().text))
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::new(1.0_f32, pal().border))
                        .rounding(Rounding::ZERO),
                )
                .clicked()
            {
                let _ = std::process::Command::new("xdg-open")
                    .arg(model::exports_dir())
                    .spawn();
            }
        });
    }

    fn do_export(&mut self, zip: bool) {
        let Some(id) = self.export_playlist.clone() else { return };
        let Some(pl) = self.state.playlist(&id).cloned() else { return };
        let tracks = self.state.tracks_of(&pl);
        let mut path = std::path::PathBuf::from(self.export_path.trim());
        if path.as_os_str().is_empty() {
            path = std::path::PathBuf::from(default_export_path(&pl.name, !zip));
        }
        let ext = if zip { "zip" } else { "m3u" };
        if path.extension().and_then(|e| e.to_str()) != Some(ext) {
            path.set_extension(ext);
        }
        let result = if zip {
            transfer::export_zip(&path, &pl, &tracks, self.include_audio)
        } else {
            transfer::export_m3u(&path, &pl, &tracks)
        };
        match result {
            Ok(()) => {
                self.status = format!("Экспортировано: {}", path.display());
                self.log.push(self.status.clone());
                self.export_path = path.to_string_lossy().to_string();
            }
            Err(e) => {
                self.status = format!("Ошибка экспорта: {e}");
                self.log.push(self.status.clone());
            }
        }
    }

    fn do_import(&mut self) {
        let path = std::path::PathBuf::from(self.import_path.trim());
        if !path.is_file() {
            self.status = format!("Файл не найден: {}", path.display());
            return;
        }
        let is_zip = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("zip"))
            .unwrap_or(false);

        let (name, files) = if is_zip {
            match transfer::import_zip(&path) {
                Ok(v) => v,
                Err(e) => {
                    self.status = format!("Импорт не удался: {e}");
                    return;
                }
            }
        } else {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "Импорт".into());
            match transfer::read_m3u(&path) {
                Ok(files) => (name, files),
                Err(e) => {
                    self.status = format!("Импорт не удался: {e}");
                    return;
                }
            }
        };

        let mut added_to_library = 0usize;
        let mut ids: Vec<String> = Vec::new();
        for file in files {
            if !file.is_file() {
                // Пробуем найти по имени в уже просканированной библиотеке.
                if let Some(found) = self.state.tracks.iter().find(|t| {
                    t.file_name() == file.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default()
                }) {
                    ids.push(found.id.clone());
                }
                continue;
            }
            let path_str = file.to_string_lossy().to_string();
            let id = if let Some(existing) = self.state.find_by_path(&path_str) {
                existing.id.clone()
            } else {
                let track = crate::library::make_track(&file);
                let id = track.id.clone();
                self.state.tracks.push(track);
                added_to_library += 1;
                id
            };
            if !ids.contains(&id) {
                ids.push(id);
            }
        }

        if ids.is_empty() {
            self.status = "В плейлисте не нашлось доступных треков".into();
            return;
        }

        // id только из имени и числа треков совпадал при повторном импорте того же
        // файла — два плейлиста с одним id, и второй оказывался недоступен.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut playlist = Playlist {
            id: short_id(&format!("{name}:{}:{stamp}", ids.len())),
            name: name.clone(),
            track_ids: ids.clone(),
        };
        // если имя занято — добавим суффикс
        let mut n = 2;
        while self.state.playlists.iter().any(|p| p.name == playlist.name) {
            playlist.name = format!("{name} ({n})");
            n += 1;
        }
        self.state.playlists.push(playlist.clone());
        self.persist();
        self.tab = Tab::Playlist(playlist.id.clone());
        self.status = format!(
            "Импортировано: {} треков ({} новых в библиотеке)",
            ids.len(),
            added_to_library
        );
        self.log.push(self.status.clone());
    }

    /// Компактная строка синхронизации на главной (когда плагин включён).
    fn sync_strip(&mut self, ui: &mut egui::Ui) {
        let mut open_settings = false;
        let mut quick_sync = false;
        let busy = self.busy.is_some();
        let peer_ready = !self.settings.peer.trim().is_empty();
        egui::Frame::none()
            .fill(pal().card)
            .stroke(Stroke::new(1.0_f32, pal().border))
            .inner_margin(egui::Margin::symmetric(12.0, 8.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    status_dot(
                        ui,
                        if self.server_error.is_some() {
                            pal().danger
                        } else {
                            pal().accent
                        },
                    );
                    ui.label(
                        RichText::new("СИНХРОНИЗАЦИЯ")
                            .font(mono_font(9.5))
                            .strong()
                            .color(pal().accent2),
                    );
                    ui.label(mono(self.lan_url(), 10.0, pal().muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(mono("[ НАСТРОЙКИ ]", 9.5, pal().muted))
                                    .frame(false)
                                    .rounding(Rounding::ZERO),
                            )
                            .clicked()
                        {
                            open_settings = true;
                        }
                        let btn = egui::Button::new(mono(
                            if busy { "ОБМЕН…" } else { "[ ОБМЕН ]" },
                            9.5,
                            if peer_ready { pal().bg } else { pal().muted },
                        ))
                        .fill(if peer_ready { pal().accent2 } else { Color32::TRANSPARENT })
                        .stroke(Stroke::new(1.0_f32, pal().border))
                        .rounding(Rounding::ZERO);
                        if ui.add_enabled(!busy, btn).clicked() {
                            quick_sync = true;
                        }
                        if !peer_ready {
                            ui.label(mono("адрес второго устройства не задан", 9.0, pal().muted));
                        }
                    });
                });
            });
        if open_settings {
            self.open_settings(SettingsTab::Sync);
        }
        if quick_sync {
            if peer_ready {
                self.start_sync();
            } else {
                self.open_settings(SettingsTab::Sync);
            }
        }
    }

    /// Модальное окно настроек поверх плеера.
    fn settings_overlay(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let screen = ctx.screen_rect();
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.settings_open = false;
            self.save_settings();
            return;
        }

        let card_size = egui::vec2(
            (screen.width() - 140.0).clamp(600.0, 900.0),
            (screen.height() - 140.0).clamp(420.0, 620.0),
        );
        let card = egui::Rect::from_center_size(screen.center(), card_size);
        egui::Area::new(egui::Id::new("settings-backdrop"))
            .fixed_pos(screen.min)
            .order(egui::Order::Foreground)
            .interactable(false)
            .show(ctx, |ui| {
                let (rect, _) = ui.allocate_exact_size(screen.size(), egui::Sense::hover());
                ui.painter()
                    .rect_filled(rect, Rounding::ZERO, Color32::from_black_alpha(198));
            });

        if self.settings_just_opened {
            self.settings_just_opened = false;
        } else {
            let outside_click = ctx.input(|i| {
                i.pointer.button_clicked(egui::PointerButton::Primary)
                    && i.pointer
                        .interact_pos()
                        .map(|pos| !card.contains(pos))
                        .unwrap_or(false)
            });
            if outside_click {
                self.settings_open = false;
                self.save_settings();
                return;
            }
        }

        let mut close = false;
        egui::Window::new("settings")
            .id(egui::Id::new("settings-window"))
            .title_bar(false)
            .resizable(false)
            .collapsible(false)
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .fixed_size(card_size)
            .frame(
                egui::Frame::none()
                    .fill(pal().panel)
                    .stroke(Stroke::new(1.0_f32, pal().border)),
            )
            .show(ctx, |ui| {
                let (bar, _) = ui.allocate_exact_size(
                    Vec2::new(ui.available_width(), 2.0),
                    egui::Sense::hover(),
                );
                ui.painter().rect_filled(bar, Rounding::ZERO, pal().accent);

                egui::Frame::none()
                    .inner_margin(egui::Margin::symmetric(16.0, 8.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("НАСТРОЙКИ")
                                    .font(mono_font(13.0))
                                    .strong()
                                    .color(pal().text),
                            );
                            ui.label(mono("mpt.mp", 10.0, pal().accent));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .add(
                                            egui::Button::new(mono("ЗАКРЫТЬ", 9.5, pal().muted))
                                                .frame(false)
                                                .rounding(Rounding::ZERO),
                                        )
                                        .clicked()
                                    {
                                        close = true;
                                    }
                                    ui.label(mono("ESC", 9.0, pal().muted));
                                },
                            );
                        });
                    });

                settings_hline(ui);

                let body_height = (card_size.y - 80.0).max(120.0);
                let body_width = (card_size.x - 182.0).max(240.0);
                ui.horizontal_top(|ui| {
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        ui.add_space(6.0);
                        for tab in SETTINGS_TABS {
                            let active = self.settings_tab == tab;
                            let (tab_rect, resp) = ui.allocate_exact_size(
                                Vec2::new(150.0, 30.0),
                                egui::Sense::click(),
                            );
                            let resp =
                                resp.on_hover_cursor(egui::CursorIcon::PointingHand);
                            if active {
                                ui.painter().rect_filled(tab_rect, Rounding::ZERO, pal().card);
                                ui.painter().rect_filled(
                                    egui::Rect::from_min_size(
                                        tab_rect.min,
                                        Vec2::new(2.0, tab_rect.height()),
                                    ),
                                    Rounding::ZERO,
                                    pal().accent,
                                );
                            } else if resp.hovered() {
                                ui.painter().rect_filled(
                                    tab_rect,
                                    Rounding::ZERO,
                                    alpha(pal().card_hi, 130),
                                );
                            }
                            ui.painter().text(
                                egui::pos2(tab_rect.left() + 12.0, tab_rect.center().y),
                                egui::Align2::LEFT_CENTER,
                                tab.label(),
                                mono_font(10.0),
                                if active { pal().text } else { pal().muted },
                            );
                            if resp.clicked() {
                                self.settings_tab = tab;
                            }
                        }
                        ui.add_space(6.0);
                    });

                    ui.visuals_mut().widgets.noninteractive.bg_stroke =
                        Stroke::new(1.0_f32, pal().border);
                    ui.separator();

                    ui.allocate_ui_with_layout(
                        Vec2::new(body_width, body_height),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            egui::ScrollArea::vertical()
                                .id_salt("settings-scroll")
                                .auto_shrink([false, false])
                                .max_height(body_height)
                                .show(ui, |ui| {
                                    ui.set_width(body_width - 18.0);
                                    ui.add_space(6.0);
                                    ui.spacing_mut().item_spacing.y = 8.0;
                                    match self.settings_tab {
                                        SettingsTab::Sound => self.settings_sound(ui),
                                        SettingsTab::Sync => self.settings_sync(ui, ctx),
                                        SettingsTab::Library => self.settings_library(ui),
                                        SettingsTab::Keys => settings_keys(ui),
                                        SettingsTab::Look => self.settings_look(ui),
                                    }
                                    ui.add_space(16.0);
                                });
                        },
                    );
                });
            });

        if close {
            self.settings_open = false;
            self.save_settings();
        }
    }

    fn settings_sound(&mut self, ui: &mut egui::Ui) {
        let width = ui.available_width();
        section_title(ui, "ГРОМКОСТЬ");
        let mut volume = self.settings.volume;
        let mut volume_changed = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = (width - 130.0).max(120.0);
            let resp = ui.add(
                egui::Slider::new(&mut volume, 0.0..=1.0)
                    .show_value(false)
                    .trailing_fill(true),
            );
            if resp.changed() {
                volume_changed = true;
            }
            ui.label(mono(format!("{:>3.0}%", volume * 100.0), 10.0, pal().muted));
            ui.label(mono("↑ ↓", 9.0, pal().muted))
                .on_hover_text("Громкость стрелками, не закрывая настройки");
        });
        if volume_changed {
            self.settings.volume = volume;
            if let Some(player) = &mut self.player {
                player.set_volume(volume);
            }
            self.settings_dirty = true;
        }

        ui.add_space(14.0);
        section_title(ui, "ПАНОРАМА");
        let mut pan = self.settings.pan;
        let mut pan_changed = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().slider_width = (width - 170.0).max(120.0);
            let resp = ui.add(
                egui::Slider::new(&mut pan, -1.0..=1.0)
                    .show_value(false)
                    .trailing_fill(true),
            );
            if resp.changed() {
                pan_changed = true;
            }
            ui.label(mono(pan_label(pan), 10.0, pal().muted));
            if ui
                .add(
                    egui::Button::new(mono("СБРОС", 9.0, pal().muted))
                        .frame(false)
                        .rounding(Rounding::ZERO),
                )
                .clicked()
            {
                pan = 0.0;
                pan_changed = true;
            }
        });
        if pan_changed {
            self.settings.pan = pan;
            self.settings_dirty = true;
        }

        ui.add_space(16.0);
        let mut eq_enabled = self.settings.eq_enabled;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("ЭКВАЛАЙЗЕР")
                    .font(mono_font(9.0))
                    .strong()
                    .color(pal().muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                toggle_switch(ui, &mut eq_enabled);
                ui.label(mono(
                    if eq_enabled { "ВКЛ" } else { "ВЫКЛ" },
                    9.0,
                    if eq_enabled { pal().accent } else { pal().muted },
                ));
            });
        });
        settings_hline(ui);
        if eq_enabled != self.settings.eq_enabled {
            self.settings.eq_enabled = eq_enabled;
            self.settings_dirty = true;
        }

        let mut gains = self.settings.eq_array();
        let mut eq_changed = false;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (name, preset) in PRESETS {
                if ui
                    .add(
                        egui::Button::new(mono(name, 8.5, pal().text))
                            .frame(false)
                            .rounding(Rounding::ZERO),
                    )
                    .clicked()
                {
                    gains = preset;
                    eq_changed = true;
                }
            }
        });
        ui.add_space(6.0);

        let enabled = self.settings.eq_enabled;
        let column = ((width - 24.0) / EQ_BANDS as f32).max(26.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.spacing_mut().slider_width = 104.0;
            for band in 0..EQ_BANDS {
                ui.vertical(|ui| {
                    ui.set_width(column - 4.0);
                    let slider = egui::Slider::new(&mut gains[band], EQ_MIN..=EQ_MAX)
                        .vertical()
                        .show_value(false)
                        .trailing_fill(true);
                    let resp = ui
                        .add_enabled_ui(enabled, |ui| {
                            ui.add_sized(
                                Vec2::new((column - 8.0).max(18.0), 116.0),
                                slider,
                            )
                        })
                        .inner;
                    if resp.changed() {
                        eq_changed = true;
                    }
                    ui.label(mono(
                        format!("{:+}", gains[band].round() as i32),
                        8.5,
                        if enabled { pal().text } else { pal().muted },
                    ));
                    ui.label(mono(EQ_LABELS[band], 8.5, pal().muted));
                });
            }
        });
        if eq_changed {
            self.settings.eq = gains.to_vec();
            self.settings_dirty = true;
        }
        ui.label(mono(
            "10 полос, ±12 дБ · изменения слышны сразу",
            9.5,
            pal().muted,
        ));
    }

    fn settings_sync(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let mut enabled = self.settings.sync_enabled;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("ПЛАГИН СИНХРОНИЗАЦИИ")
                    .font(mono_font(9.0))
                    .strong()
                    .color(pal().muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                toggle_switch(ui, &mut enabled);
                ui.label(mono(
                    if enabled { "ВКЛЮЧЁН" } else { "ВЫКЛЮЧЕН" },
                    9.0,
                    if enabled { pal().accent2 } else { pal().muted },
                ));
            });
        });
        settings_hline(ui);
        if enabled != self.settings.sync_enabled {
            self.settings.sync_enabled = enabled;
            self.settings_dirty = true;
        }

        if !self.settings.sync_enabled {
            ui.label(mono(
                "Плагин выключен. Включи — на главной появится строка синхронизации.",
                10.0,
                pal().muted,
            ));
            ui.label(mono(
                "Здесь же адрес этого устройства и обмен треками со вторым компьютером.",
                10.0,
                pal().muted,
            ));
            ui.add_space(8.0);
            ui.label(mono(
                "Сервер LAN работает всегда: позже к нему подключится Android-приложение.",
                9.5,
                alpha(pal().muted, 180),
            ));
            return;
        }

        let width = ui.available_width();
        ui.add_space(4.0);
        card(ui, width, |ui| {
            ui.label(
                RichText::new("ЭТО УСТРОЙСТВО")
                    .font(mono_font(9.0))
                    .strong()
                    .color(pal().accent),
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new(model::device_name())
                    .font(mono_font(13.0))
                    .color(pal().text),
            );
            ui.horizontal(|ui| {
                ui.label(mono(self.lan_url(), 10.5, pal().muted));
                if ui
                    .add(
                        egui::Button::new(mono("СКОПИРОВАТЬ", 9.0, pal().muted))
                            .frame(false)
                            .rounding(Rounding::ZERO),
                    )
                    .clicked()
                {
                    ctx.copy_text(self.lan_url());
                    self.status = "Адрес скопирован".into();
                }
            });
        });

        ui.add_space(8.0);
        let mut peer = self.settings.peer.clone();
        let mut peer_changed = false;
        let mut sync_now = false;
        card(ui, width, |ui| {
            ui.label(
                RichText::new("ВТОРОЕ УСТРОЙСТВО")
                    .font(mono_font(9.0))
                    .strong()
                    .color(pal().accent2),
            );
            ui.add_space(2.0);
            ui.label(mono(
                "запусти mpt.mp на другом компьютере и вставь его адрес",
                9.5,
                pal().muted,
            ));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut peer)
                        .hint_text("192.168.1.50:8787")
                        .desired_width(260.0),
                );
                if resp.changed() {
                    peer_changed = true;
                }
                let busy = self.busy.is_some();
                let ready = !peer.trim().is_empty();
                let btn = egui::Button::new(mono(
                    "СИНХРОНИЗИРОВАТЬ",
                    10.0,
                    if ready { pal().bg } else { pal().muted },
                ))
                .fill(if ready { pal().accent2 } else { pal().card_hi })
                .rounding(Rounding::ZERO);
                if ui.add_enabled(!busy && ready, btn).clicked() {
                    sync_now = true;
                }
            });
            ui.label(mono(
                "двусторонне: недостающие треки скачаются, новые отправятся",
                9.5,
                pal().muted,
            ));
            if let Some(err) = &self.server_error {
                ui.label(mono(format!("сервер: {err}"), 9.5, pal().danger));
            }
        });
        if peer_changed {
            self.settings.peer = peer;
            self.settings_dirty = true;
        }

        ui.add_space(8.0);
        let mut clear_log = false;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new("ЖУРНАЛ")
                    .font(mono_font(9.0))
                    .strong()
                    .color(pal().muted),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(mono("ОЧИСТИТЬ", 9.0, pal().muted))
                            .frame(false)
                            .rounding(Rounding::ZERO),
                    )
                    .clicked()
                {
                    clear_log = true;
                }
            });
        });
        egui::Frame::none()
            .fill(pal().card)
            .stroke(Stroke::new(1.0_f32, pal().border))
            .inner_margin(egui::Margin::same(8.0))
            .show(ui, |ui| {
                ui.set_width((width - 18.0).max(80.0));
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    .max_height(120.0)
                    .show(ui, |ui| {
                        if self.log.is_empty() {
                            ui.label(mono("пока пусто", 10.0, pal().muted));
                        }
                        for line in &self.log {
                            ui.label(mono(line.clone(), 10.0, pal().text));
                        }
                    });
            });
        ui.label(mono(
            "QR-код и мобильная веб-версия убраны — вместо них будет Android-приложение.",
            9.0,
            alpha(pal().muted, 170),
        ));

        if clear_log {
            self.log.clear();
        }
        if sync_now {
            self.save_settings();
            self.start_sync();
        }
    }

    fn settings_library(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "ПАПКИ С МУЗЫКОЙ");
        let folders = self.state.folders.clone();
        if folders.is_empty() {
            ui.label(mono("пока не добавлено ни одной папки", 10.0, pal().muted));
        }
        for folder in &folders {
            ui.horizontal(|ui| {
                folder_icon(ui, pal().accent2);
                ui.label(
                    RichText::new(folder)
                        .font(mono_font(11.0))
                        .color(pal().text),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(mono("УБРАТЬ", 9.0, pal().muted))
                                .frame(false)
                                .rounding(Rounding::ZERO),
                        )
                        .clicked()
                    {
                        self.state.folders.retain(|f| f != folder);
                        self.persist();
                        self.start_scan();
                    }
                });
            });
        }

        ui.add_space(10.0);
        settings_hline(ui);
        ui.label(
            RichText::new("ДОБАВИТЬ ПАПКУ")
                .font(mono_font(9.0))
                .strong()
                .color(pal().muted),
        );
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.new_folder).desired_width(320.0));
            if ui
                .add(
                    egui::Button::new(mono("ДОБАВИТЬ", 10.0, pal().bg))
                        .fill(pal().accent)
                        .rounding(Rounding::ZERO),
                )
                .clicked()
            {
                let path = self.new_folder.trim().to_string();
                if std::path::Path::new(&path).is_dir() && !self.state.folders.contains(&path) {
                    self.state.folders.push(path);
                    self.persist();
                    self.start_scan();
                } else {
                    self.status = "Папка не найдена".into();
                }
            }
            if ui
                .add(
                    egui::Button::new(mono("~/MUSIC", 10.0, pal().text))
                        .fill(Color32::TRANSPARENT)
                        .stroke(Stroke::new(1.0_f32, pal().border))
                        .rounding(Rounding::ZERO),
                )
                .clicked()
            {
                if let Some(home) = dirs::home_dir() {
                    let music = home.join("Music");
                    let path = music.to_string_lossy().to_string();
                    if music.is_dir() && !self.state.folders.contains(&path) {
                        self.state.folders.push(path);
                        self.new_folder = music.to_string_lossy().to_string();
                        self.persist();
                        self.start_scan();
                    }
                }
            }
        });
        ui.label(mono(
            format!("synced: {}", model::synced_dir().display()),
            9.0,
            pal().muted,
        ));
        ui.add_space(6.0);
        if ui
            .add(
                egui::Button::new(mono("ОБНОВИТЬ БИБЛИОТЕКУ", 10.0, pal().text))
                    .fill(Color32::TRANSPARENT)
                    .stroke(Stroke::new(1.0_f32, pal().border))
                    .rounding(Rounding::ZERO),
            )
            .clicked()
        {
            self.start_scan();
        }
    }

    fn settings_look(&mut self, ui: &mut egui::Ui) {
        section_title(ui, "ТЕМА");
        ui.label(
            RichText::new(format!("Omarchy · {}", pal().name))
                .font(mono_font(13.0))
                .color(pal().text),
        );
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for color in [
                pal().bg,
                pal().panel,
                pal().card,
                pal().border,
                pal().muted,
                pal().text,
                pal().accent,
                pal().accent2,
                pal().danger,
                pal().warn,
            ] {
                let (rect, _) =
                    ui.allocate_exact_size(Vec2::splat(22.0), egui::Sense::hover());
                ui.painter().rect_filled(rect, Rounding::ZERO, color);
                ui.painter()
                    .rect_stroke(rect, Rounding::ZERO, Stroke::new(1.0_f32, pal().border));
            }
        });
        ui.label(mono(
            "палитра читается из omarchy-theme-current, смена темы в системе меняет плеер",
            9.5,
            pal().muted,
        ));

        ui.add_space(14.0);
        section_title(ui, "ИНТЕРФЕЙС");
        let mut visualizer = self.settings.visualizer;
        ui.horizontal(|ui| {
            toggle_switch(ui, &mut visualizer);
            ui.label(mono(
                "визуализатор на фоне библиотеки",
                10.0,
                if visualizer { pal().text } else { pal().muted },
            ));
        });
        if visualizer != self.settings.visualizer {
            self.settings.visualizer = visualizer;
            self.settings_dirty = true;
        }
        ui.label(mono(
            "C — обложка на весь экран, S — это меню, Esc — закрыть",
            9.5,
            pal().muted,
        ));
    }

}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.handle_keys(ctx);
        self.poll_background(ctx);
        if let Some(player) = &mut self.player {
            player.tick();
        }
        self.request_cover();
        self.animate_cover(ctx);
        let fx = self.fx_params();
        if let Some(player) = &mut self.player {
            player.set_fx(fx);
        }
        if let Some(err) = self.player.as_ref().and_then(|p| p.error.clone()) {
            if self.status != err {
                self.status = err.clone();
                self.log.push(err);
            }
        }
        self.update_levels(ctx);
        self.header(ctx);
        self.sidebar(ctx);
        self.status_bar(ctx);
        self.player_bar(ctx);
        self.central(ctx);
        self.cover_overlay(ctx);
        self.settings_overlay(ctx);
        self.rename_dialog(ctx);
        if self.settings_dirty && self.settings_saved_at.elapsed().as_secs_f32() > 0.7 {
            self.save_settings();
        }
    }
}

fn setup_theme(ctx: &egui::Context) {
    ctx.set_fonts(fonts());

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = pal().panel;
    visuals.window_fill = pal().panel;
    visuals.extreme_bg_color = pal().bg;
    visuals.faint_bg_color = pal().card;
    visuals.window_stroke = Stroke::new(1.0_f32, pal().border);
    visuals.widgets.noninteractive.bg_fill = pal().panel;
    visuals.widgets.noninteractive.weak_bg_fill = pal().panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, pal().border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, pal().muted);
    visuals.widgets.inactive.bg_fill = pal().card;
    visuals.widgets.inactive.weak_bg_fill = pal().card;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, pal().border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, pal().text);
    visuals.widgets.hovered.bg_fill = pal().card_hi;
    visuals.widgets.hovered.weak_bg_fill = pal().card_hi;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, pal().accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, pal().text);
    visuals.widgets.active.bg_fill = pal().card_hi;
    visuals.widgets.active.weak_bg_fill = pal().card_hi;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, pal().accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0_f32, pal().text);
    for w in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        w.rounding = Rounding::ZERO;
    }
    visuals.selection.bg_fill = alpha(pal().accent, 60);
    visuals.selection.stroke = Stroke::new(1.0_f32, pal().accent);
    visuals.hyperlink_color = pal().accent;
    visuals.window_rounding = Rounding::ZERO;
    visuals.menu_rounding = Rounding::ZERO;
    visuals.popup_shadow = egui::epaint::Shadow::NONE;
    visuals.window_shadow = egui::epaint::Shadow::NONE;
    ctx.set_visuals(visuals);

    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.button_padding = Vec2::new(10.0, 6.0);
    style.spacing.interact_size.y = 24.0;
    style.spacing.slider_width = 160.0;
    style.text_styles = [
        (egui::TextStyle::Small, mono_font(10.0)),
        (egui::TextStyle::Body, mono_font(12.5)),
        (egui::TextStyle::Button, mono_font(12.0)),
        (egui::TextStyle::Heading, mono_font(18.0)),
        (egui::TextStyle::Monospace, mono_font(12.0)),
    ]
    .into();
    ctx.set_style(style);
}

fn fonts() -> egui::FontDefinitions {
    let mut fonts = egui::FontDefinitions::default();

    // Пиксельный шрифт для названия приложения.
    fonts.font_data.insert(
        "pixel".to_owned(),
        egui::FontData::from_static(include_bytes!("../assets/fonts/Jersey10-Regular.ttf")),
    );
    fonts.families.insert(
        egui::FontFamily::Name("pixel".into()),
        vec!["pixel".to_owned()],
    );

    // Моноширинный шрифт системного вида (Omarchy использует JetBrainsMono NF).
    let mono_paths = [
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
        "/usr/share/fonts/TTF/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/jetbrains-mono/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
    ];
    for path in mono_paths {
        if let Ok(data) = std::fs::read(path) {
            fonts
                .font_data
                .insert("mono".to_owned(), egui::FontData::from_owned(data));
            for family in [egui::FontFamily::Monospace, egui::FontFamily::Proportional] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .insert(0, "mono".to_owned());
            }
            break;
        }
    }

    // Основной пропорциональный шрифт — для кириллицы и длинных подписей.
    if let Some((name, data)) = find_system_font() {
        fonts
            .font_data
            .insert(name.clone(), egui::FontData::from_owned(data));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            let list = fonts.families.entry(family).or_default();
            if !list.contains(&name) {
                list.push(name.clone());
            }
        }
    }

    fonts
}

fn mono_font(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Monospace)
}

fn pixel(size: f32) -> egui::FontId {
    egui::FontId::new(size, egui::FontFamily::Name("pixel".into()))
}

fn mono(text: impl Into<String>, size: f32, color: Color32) -> RichText {
    RichText::new(text).font(mono_font(size)).color(color)
}

/// Заголовок страницы в моноширинном начертании.
fn page_title(ui: &mut egui::Ui, text: &str) {
    ui.label(
        RichText::new(text)
            .font(mono_font(19.0))
            .strong()
            .color(pal().text),
    );
}


fn find_system_font() -> Option<(String, Vec<u8>)> {
    let candidates = [
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/NotoSans-Regular.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
    ];
    for path in candidates {
        if let Ok(data) = std::fs::read(path) {
            return Some(("system".to_string(), data));
        }
    }
    let home_fonts = dirs::home_dir()?.join(".local/share/fonts");
    for entry in walkdir::WalkDir::new(home_fonts).max_depth(3).into_iter().flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) == Some("ttf") {
            if let Ok(data) = std::fs::read(entry.path()) {
                return Some(("system".to_string(), data));
            }
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IconKind {
    Play,
    Pause,
    Prev,
    Next,
}

fn transport_button(
    ui: &mut egui::Ui,
    kind: IconKind,
    size: f32,
    fill: Option<Color32>,
    tint: Color32,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), egui::Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let painter = ui.painter();
    let center = rect.center();
    let box_rect = rect.shrink(0.5);
    if let Some(fill) = fill {
        painter.rect_filled(box_rect, Rounding::ZERO, fill);
    } else if resp.hovered() {
        painter.rect_filled(box_rect, Rounding::ZERO, pal().card_hi);
        painter.rect_stroke(
            box_rect,
            Rounding::ZERO,
            Stroke::new(1.0_f32, pal().border),
        );
    }
    let s = size * 0.27;
    match kind {
        IconKind::Play => {
            let pts = vec![
                center + Vec2::new(-s * 0.45, -s * 0.95),
                center + Vec2::new(-s * 0.45, s * 0.95),
                center + Vec2::new(s * 0.95, 0.0),
            ];
            painter.add(egui::Shape::convex_polygon(pts, tint, Stroke::NONE));
        }
        IconKind::Pause => {
            let w = s * 0.34;
            let h = s * 1.8;
            for dx in [-s * 0.55, s * 0.2] {
                painter.rect_filled(
                    egui::Rect::from_center_size(
                        center + Vec2::new(dx + w * 0.5, 0.0),
                        Vec2::new(w, h),
                    ),
                    Rounding::ZERO,
                    tint,
                );
            }
        }
        IconKind::Prev | IconKind::Next => {
            let dir = if kind == IconKind::Next { 1.0 } else { -1.0 };
            for offset in [-0.45f32, 0.4] {
                let base = center + Vec2::new(dir * offset * s, 0.0);
                let pts = vec![
                    base + Vec2::new(-dir * s * 0.4, -s * 0.9),
                    base + Vec2::new(-dir * s * 0.4, s * 0.9),
                    base + Vec2::new(dir * s * 0.5, 0.0),
                ];
                painter.add(egui::Shape::convex_polygon(pts, tint, Stroke::NONE));
            }
        }
    }
    resp
}

fn volume_label(volume: f32) -> String {
    format!("VOL {:.0}%", volume * 100.0)
}

fn paint_volume_icon(painter: &egui::Painter, rect: egui::Rect, color: Color32) {
    let c = rect.center();
    let s = 7.0;
    painter.rect_filled(
        egui::Rect::from_center_size(c + Vec2::new(-s * 0.45, 0.0), Vec2::new(s * 0.5, s * 0.55)),
        Rounding::ZERO,
        color,
    );
    let cone = vec![
        c + Vec2::new(-s * 0.25, 0.0),
        c + Vec2::new(s * 0.18, -s * 0.62),
        c + Vec2::new(s * 0.18, s * 0.62),
    ];
    painter.add(egui::Shape::convex_polygon(cone, color, Stroke::NONE));
    for (radius, span) in [(s * 0.45f32, 0.75f32), (s * 0.78, 0.62)] {
        let mut pts = Vec::new();
        for i in 0..=10 {
            let a = -span + 2.0 * span * (i as f32 / 10.0);
            pts.push(c + Vec2::new(s * 0.18 + radius * a.cos() * 0.85, radius * a.sin()));
        }
        painter.add(egui::Shape::line(pts, Stroke::new(1.4_f32, color)));
    }
}

fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(10.0, 10.0), egui::Sense::hover());
    ui.painter().rect_filled(
        egui::Rect::from_center_size(rect.center(), Vec2::splat(6.0)),
        Rounding::ZERO,
        color,
    );
}

fn folder_icon(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(16.0, 16.0), egui::Sense::hover());
    let painter = ui.painter();
    let top_left = rect.center() - Vec2::new(7.0, 5.5);
    painter.rect_filled(
        egui::Rect::from_min_size(top_left, Vec2::new(6.0, 3.0)),
        Rounding::ZERO,
        color.gamma_multiply(0.7),
    );
    painter.rect_filled(
        egui::Rect::from_min_size(top_left + Vec2::new(0.0, 2.0), Vec2::new(14.0, 9.0)),
        Rounding::ZERO,
        color,
    );
}

fn track_subtitle(track: &Track) -> String {
    match (track.artist.is_empty(), track.album.is_empty()) {
        (true, true) => track.file_name(),
        (false, true) => track.artist.clone(),
        (true, false) => track.album.clone(),
        (false, false) => format!("{} — {}", track.artist, track.album),
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0).max(1e-5)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn paint_vertical_gradient(
    painter: &egui::Painter,
    rect: egui::Rect,
    top: Color32,
    bottom: Color32,
) {
    // Полосы вместо вершинного меша — предсказуемо рисуется в любом бэкенде.
    const STEPS: usize = 56;
    for i in 0..STEPS {
        let t0 = i as f32 / STEPS as f32;
        let t1 = (i + 1) as f32 / STEPS as f32;
        let color = mix_rgba(top, bottom, (t0 + t1) * 0.5);
        if color.a() == 0 {
            continue;
        }
        let y0 = rect.top() + rect.height() * t0;
        let y1 = rect.top() + rect.height() * t1;
        painter.rect_filled(
            egui::Rect::from_min_max(
                egui::pos2(rect.left(), y0),
                egui::pos2(rect.right(), y1 + 0.5),
            ),
            Rounding::ZERO,
            color,
        );
    }
}

fn mix_rgba(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let f = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_unmultiplied(
        f(a.r(), b.r()),
        f(a.g(), b.g()),
        f(a.b(), b.b()),
        f(a.a(), b.a()),
    )
}

fn sample_thumb(img: &egui::ColorImage, u: f32, v: f32) -> Color32 {
    let w = img.size[0].max(1);
    let h = img.size[1].max(1);
    let x = ((u.clamp(0.0, 0.9999) * w as f32) as usize).min(w - 1);
    let y = ((v.clamp(0.0, 0.9999) * h as f32) as usize).min(h - 1);
    img.pixels[y * w + x]
}

/// Обрезает длинный текст по ширине (моноширинный шрифт).
fn fit_text(text: &str, size: f32, max_width: f32) -> String {
    let max_chars = (max_width / (size * 0.62)).max(4.0) as usize;
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Спектр на фоне библиотеки: узкие вертикальные полосы снизу вверх.
fn paint_visualizer(ui: &egui::Ui, levels: &[f32]) {
    if levels.iter().all(|v| *v <= 0.004) {
        return;
    }
    let rect = ui.max_rect();
    let painter = ui.painter();
    let n = levels.len().max(1);
    let slot = rect.width() / n as f32;
    let gap = (slot * 0.34).clamp(1.5, 5.0);
    let max_h = rect.height() * 0.74;
    let base = rect.bottom();

    for (i, level) in levels.iter().enumerate() {
        let v = level.clamp(0.0, 1.0).powf(0.85);
        let h = (v * max_h).max(1.5);
        let x0 = rect.left() + i as f32 * slot + gap * 0.5;
        let x1 = rect.left() + (i + 1) as f32 * slot - gap * 0.5;
        if x1 <= x0 {
            continue;
        }
        // Градиент: ярче к вершине полосы.
        let steps = 7usize;
        let seg_h = h / steps as f32;
        for s in 0..steps {
            let t = s as f32 / (steps - 1) as f32;
            let y0 = base - (s + 1) as f32 * seg_h;
            let bar = egui::Rect::from_min_max(
                egui::pos2(x0, y0),
                egui::pos2(x1, (y0 + seg_h + 0.5).min(base)),
            );
            let a = 8.0 + v * 62.0 * (0.12 + 0.88 * t);
            painter.rect_filled(bar, Rounding::ZERO, alpha(pal().accent, a as u8));
        }
        let cap = egui::Rect::from_min_max(
            egui::pos2(x0, base - h),
            egui::pos2(x1, (base - h + 2.0).min(base)),
        );
        painter.rect_filled(
            cap,
            Rounding::ZERO,
            alpha(pal().accent, (55.0 + v * 155.0) as u8),
        );
    }
    painter.hline(
        rect.x_range(),
        base,
        Stroke::new(1.0_f32, alpha(pal().accent, 80)),
    );
}


fn settings_hline(ui: &mut egui::Ui) {
    ui.add_space(4.0);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 1.0),
        egui::Sense::hover(),
    );
    ui.painter()
        .rect_filled(rect, Rounding::ZERO, alpha(pal().border, 170));
    ui.add_space(4.0);
}

/// Квадратный переключатель в духе Omarchy.
fn toggle_switch(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(40.0, 18.0), egui::Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        Rounding::ZERO,
        if *on { pal().accent } else { pal().card_hi },
    );
    painter.rect_stroke(rect, Rounding::ZERO, Stroke::new(1.0_f32, pal().border));
    let knob_x = if *on { rect.right() - 10.0 } else { rect.left() + 10.0 };
    painter.rect_filled(
        egui::Rect::from_center_size(
            egui::pos2(knob_x, rect.center().y),
            Vec2::new(14.0, 14.0),
        ),
        Rounding::ZERO,
        if *on { pal().bg } else { pal().muted },
    );
    if resp.clicked() {
        *on = !*on;
    }
    resp
}

fn pan_label(pan: f32) -> String {
    if pan.abs() < 0.02 {
        "ЦЕНТР".to_string()
    } else if pan < 0.0 {
        format!("L {:.0}%", -pan * 100.0)
    } else {
        format!("R {:.0}%", pan * 100.0)
    }
}

fn settings_keys(ui: &mut egui::Ui) {
    const ROWS: [(&str, &str); 11] = [
        ("ПРОБЕЛ", "играть / пауза"),
        ("N / P", "следующий / предыдущий трек"),
        ("← / →", "перемотка на 5 секунд"),
        ("↑ / ↓", "громкость ±5%"),
        ("C", "обложка на весь экран"),
        ("S", "меню настроек"),
        ("ESC", "закрыть обложку / настройки, очистить поиск"),
        ("1 / 2", "библиотека / экспорт и импорт"),
        ("ДВОЙНОЙ КЛИК", "играть трек"),
        ("ПРАВЫЙ КЛИК", "меню плейлиста"),
        ("КЛИК ПО ОБЛОЖКЕ", "развернуть обложку"),
    ];
    section_title(ui, "ГОРЯЧИЕ КЛАВИШИ");
    for (key, action) in ROWS {
        let (rect, _) = ui.allocate_exact_size(
            Vec2::new(ui.available_width(), 24.0),
            egui::Sense::hover(),
        );
        ui.painter().text(
            egui::pos2(rect.left() + 2.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            key,
            mono_font(10.0),
            pal().accent,
        );
        ui.painter().text(
            egui::pos2(rect.left() + 176.0, rect.center().y),
            egui::Align2::LEFT_CENTER,
            action,
            mono_font(10.0),
            pal().text,
        );
        ui.painter().hline(
            egui::Rangef::new(rect.left(), rect.right()),
            rect.bottom(),
            Stroke::new(1.0_f32, alpha(pal().border, 70)),
        );
    }
    ui.add_space(6.0);
    ui.label(mono(
        "в полях ввода клавиши работают как обычный текст",
        9.5,
        pal().muted,
    ));
}

fn section_title(ui: &mut egui::Ui, text: &str) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        ui.label(
            RichText::new(text)
                .font(mono_font(9.0))
                .strong()
                .color(pal().muted),
        );
        let remaining = ui.available_width() - 2.0;
        if remaining > 6.0 {
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(remaining, 8.0), egui::Sense::hover());
            ui.painter().hline(
                rect.x_range(),
                rect.center().y,
                Stroke::new(1.0_f32, alpha(pal().border, 130)),
            );
        }
    });
    ui.add_space(3.0);
}

fn nav_item(ui: &mut egui::Ui, label: &str, badge: &str, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 30.0),
        egui::Sense::click(),
    );
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let painter = ui.painter();
    if selected {
        painter.rect_filled(rect, Rounding::ZERO, alpha(pal().accent, 26));
        painter.rect_filled(
            egui::Rect::from_min_size(rect.min, Vec2::new(2.0, rect.height())),
            Rounding::ZERO,
            pal().accent,
        );
    } else if resp.hovered() {
        painter.rect_filled(rect, Rounding::ZERO, alpha(pal().card_hi, 120));
    }

    let text_color = if selected || resp.hovered() {
        pal().text
    } else {
        pal().muted
    };
    painter.text(
        egui::pos2(rect.left() + 12.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        mono_font(12.0),
        text_color,
    );
    if !badge.is_empty() {
        painter.text(
            egui::pos2(rect.right() - 10.0, rect.center().y),
            egui::Align2::RIGHT_CENTER,
            badge,
            mono_font(9.5),
            if selected { pal().accent } else { pal().muted },
        );
    }
    resp
}

fn card(ui: &mut egui::Ui, width: f32, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(pal().card)
        .stroke(Stroke::new(1.0_f32, pal().border))
        .inner_margin(egui::Margin::same(14.0))
        .show(ui, |ui| {
            ui.set_width((width - 28.0).max(80.0));
            ui.with_layout(egui::Layout::top_down(egui::Align::LEFT), add);
        });
}

fn empty_state(ui: &mut egui::Ui, state: &LibraryState) {
    egui::Frame::none()
        .fill(alpha(pal().card, 235))
        .stroke(Stroke::new(1.0_f32, pal().border))
        .inner_margin(egui::Margin::same(28.0))
        .show(ui, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(18.0);
                ui.label(RichText::new("♪").font(pixel(26.0)).color(pal().accent));
                ui.add_space(10.0);
                if state.folders.is_empty() {
                    ui.label(
                        RichText::new("добавь папку с музыкой")
                            .font(mono_font(14.0))
                            .color(pal().text),
                    );
                    ui.add_space(4.0);
                    ui.label(mono(
                        "раздел «Папки»: укажи путь к коллекции, например ~/Music",
                        10.5,
                        pal().muted,
                    ));
                } else {
                    ui.label(
                        RichText::new("здесь пока пусто")
                            .font(mono_font(14.0))
                            .color(pal().text),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        mono(
                            "нажми «Сканировать» или проверь, что в папках есть mp3/flac/ogg/wav",
                            10.5,
                            pal().muted,
                        ),
                    );
                }
                ui.add_space(18.0);
            });
        });
}

fn track_matches(t: &Track, q: &str) -> bool {
    t.title.to_lowercase().contains(q)
        || t.artist.to_lowercase().contains(q)
        || t.album.to_lowercase().contains(q)
        || t.file_name().to_lowercase().contains(q)
}

fn default_export_path(name: &str, m3u: bool) -> String {
    model::exports_dir()
        .join(format!(
            "{}.{}",
            transfer::safe_stem(name),
            if m3u { "m3u" } else { "zip" }
        ))
        .to_string_lossy()
        .to_string()
}

fn pseudo_shuffle(tracks: &mut [Track]) {
    if tracks.len() < 2 {
        return;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as usize)
        .unwrap_or(12345);
    let mut state = nanos | 1;
    for i in (1..tracks.len()).rev() {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let j = state % (i + 1);
        tracks.swap(i, j);
    }
}

