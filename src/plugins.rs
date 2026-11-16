//! Хост плагинов (API v1, см. plugins.md).
//!
//! Плагин — отдельный процесс; обмен строками JSON через stdin/stdout.
//! Модуль ничего не знает про интерфейс и плеер: он запускает процессы, разбирает
//! сообщения и отдаёт `App` запросы (`Handled::Request`), на которые тот отвечает
//! через `Host::respond`.
//!
//! Потоки: на каждый процесс — читатель stdout, читатель stderr и писатель stdin.
//! Писатель нужен, чтобы зависший плагин с полным каналом не замораживал интерфейс.

use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const API_VERSION: u32 = 1;

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);
/// Если плагин проработал дольше — счётчик перезапусков обнуляется.
const STABLE_RUN: Duration = Duration::from_secs(60);
const RESTART_DELAYS: [u64; 3] = [1, 5, 25];
const MAX_LINE: usize = 1024 * 1024;
const MAX_REQUESTS_PER_SEC: usize = 50;
const LOG_KEEP: usize = 100;
const WRITE_QUEUE: usize = 256;

pub const PERMISSIONS: [&str; 6] = [
    "player.read",
    "player.control",
    "library.read",
    "library.write",
    "settings.read",
    "settings.write",
];

/// События, на которые можно подписаться (`config_changed` приходит всегда).
const EVENTS: [&str; 4] = ["track_changed", "state", "queue_changed", "library_changed"];

/// Право, нужное для метода. `None` — метод неизвестен.
pub fn required_permission(method: &str) -> Option<&'static str> {
    Some(match method {
        "player.get_state" | "player.get_queue" => "player.read",
        "player.play" | "player.pause" | "player.toggle" | "player.next" | "player.prev"
        | "player.seek" | "player.set_volume" | "player.play_queue" => "player.control",
        "library.get_tracks"
        | "library.get_track"
        | "library.get_playlists"
        | "library.get_folders" => "library.read",
        "library.create_playlist"
        | "library.add_to_playlist"
        | "library.rescan"
        | "library.add_folder" => "library.write",
        "settings.get_theme" => "settings.read",
        "settings.set_theme" => "settings.write",
        _ => return None,
    })
}

/// Право, нужное для доставки события.
fn event_permission(event: &str) -> Option<&'static str> {
    match event {
        "track_changed" | "state" | "queue_changed" => Some("player.read"),
        "library_changed" => Some("library.read"),
        _ => None,
    }
}

// ───────────────────────────── манифест ─────────────────────────────

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Run {
    pub command: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SettingDef {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    pub default: Value,
    pub min: Option<i64>,
    pub max: Option<i64>,
    pub options: Vec<String>,
}

impl SettingDef {
    pub fn default_value(&self) -> Value {
        if !self.default.is_null() {
            return self.coerce(&self.default);
        }
        match self.kind.as_str() {
            "bool" => json!(false),
            "int" => json!(self.min.unwrap_or(0).min(self.max.unwrap_or(i64::MAX))),
            "choice" => json!(self.options.first().cloned().unwrap_or_default()),
            _ => json!(""),
        }
    }

    /// Приводит значение к типу настройки; негодное заменяется значением по умолчанию.
    pub fn coerce(&self, value: &Value) -> Value {
        match self.kind.as_str() {
            "bool" => match value.as_bool() {
                Some(b) => json!(b),
                None => json!(false),
            },
            "int" => {
                let lo = self.min.unwrap_or(i64::MIN);
                let hi = self.max.unwrap_or(i64::MAX).max(lo);
                match value.as_i64() {
                    Some(n) => json!(n.clamp(lo, hi)),
                    None => json!(0_i64.clamp(lo, hi)),
                }
            }
            "choice" => match value.as_str() {
                Some(s) if self.options.iter().any(|o| o == s) => json!(s),
                _ => json!(self.options.first().cloned().unwrap_or_default()),
            },
            _ => match value.as_str() {
                Some(s) => json!(s),
                None => json!(""),
            },
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct CommandDef {
    pub name: String,
    pub label: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api: u32,
    pub description: String,
    pub author: String,
    pub license: String,
    pub homepage: String,
    pub permissions: Vec<String>,
    pub run: Run,
    pub settings: Vec<SettingDef>,
    pub commands: Vec<CommandDef>,
    #[serde(skip)]
    pub dir: PathBuf,
}

pub fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    (2..=32).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn valid_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

impl Manifest {
    pub fn load(dir: &Path) -> Result<Manifest, String> {
        let text = std::fs::read_to_string(dir.join("plugin.json"))
            .map_err(|e| format!("plugin.json: {e}"))?;
        let mut manifest: Manifest =
            serde_json::from_str(&text).map_err(|e| format!("plugin.json: {e}"))?;
        manifest.dir = dir.to_path_buf();
        let folder = dir
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        manifest.validate(&folder)?;
        Ok(manifest)
    }

    fn validate(&self, folder: &str) -> Result<(), String> {
        if !valid_id(&self.id) {
            return Err(format!("invalid id '{}'", self.id));
        }
        if self.id != folder {
            return Err(format!("id '{}' does not match folder '{folder}'", self.id));
        }
        if self.name.trim().is_empty() {
            return Err("missing field name".into());
        }
        if !valid_version(&self.version) {
            return Err(format!(
                "version '{}' must be MAJOR.MINOR.PATCH",
                self.version
            ));
        }
        if self.api != API_VERSION {
            return Err(format!(
                "plugin targets API {}, core supports {API_VERSION}",
                self.api
            ));
        }
        for perm in &self.permissions {
            if !PERMISSIONS.contains(&perm.as_str()) {
                return Err(format!("unknown permission '{perm}'"));
            }
        }
        let Some(first) = self.run.command.first() else {
            return Err("empty run.command".into());
        };
        if first.is_empty() || self.run.command.iter().any(|a| a.contains('\0')) {
            return Err("invalid run.command".into());
        }
        if Path::new(first).is_absolute() || first.split(['/', '\\']).any(|s| s == "..") {
            return Err("run.command: program must be in PATH or inside the plugin folder".into());
        }
        let mut keys = HashSet::new();
        for s in &self.settings {
            if s.key.is_empty() || !keys.insert(s.key.as_str()) {
                return Err(format!("setting with empty or duplicate key '{}'", s.key));
            }
            match s.kind.as_str() {
                "string" | "bool" | "int" | "path" => {}
                "choice" if !s.options.is_empty() => {}
                "choice" => return Err(format!("choice '{}' has no options", s.key)),
                other => return Err(format!("unknown setting type '{other}'")),
            }
        }
        let mut names = HashSet::new();
        for c in &self.commands {
            if c.name.is_empty() || !names.insert(c.name.as_str()) {
                return Err(format!("command with empty or duplicate name '{}'", c.name));
            }
        }
        Ok(())
    }

    /// Значения по умолчанию, поверх которых лежат пользовательские (приведённые к типу).
    pub fn effective_config(&self, user: &Map<String, Value>) -> Map<String, Value> {
        let mut out = Map::new();
        for def in &self.settings {
            let value = match user.get(&def.key) {
                Some(v) => def.coerce(v),
                None => def.default_value(),
            };
            out.insert(def.key.clone(), value);
        }
        out
    }

    fn program(&self) -> String {
        let first = &self.run.command[0];
        if first.contains('/') || first.contains('\\') {
            return self.dir.join(first).to_string_lossy().to_string();
        }
        if cfg!(windows) && first == "python3" {
            return "python".into();
        }
        first.clone()
    }
}

// ───────────────────────────── сообщения ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    fn parse(s: Option<&str>) -> Level {
        match s {
            Some("warn") => Level::Warn,
            Some("error") => Level::Error,
            _ => Level::Info,
        }
    }
}

/// То, что потоки-читатели отправляют в `App`.
pub struct Incoming {
    pub id: String,
    /// Номер запуска: сообщения от уже убитого процесса игнорируются.
    pub gen: u64,
    pub kind: IncomingKind,
}

pub enum IncomingKind {
    Msg(Value),
    Log(String),
}

pub type Notify = Arc<dyn Fn(Incoming) + Send + Sync>;
pub type ApiResult = Result<Value, (&'static str, String)>;

pub struct Request {
    pub plugin: usize,
    pub id: Value,
    pub method: String,
    pub params: Value,
}

pub enum Handled {
    Nothing,
    Request(Request),
    /// Текст для строки статуса приложения.
    Status(String),
}

// ───────────────────────────── процесс ─────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunState {
    Stopped,
    Starting,
    Running,
    Stopping,
    Restarting,
    Failed,
}

struct Proc {
    child: Child,
    writer: SyncSender<String>,
    gen: u64,
    started: Instant,
    ready: bool,
    subscribed: HashSet<String>,
    calls: VecDeque<Instant>,
    stopping: Option<Instant>,
}

impl Proc {
    fn send(&self, value: &Value) -> bool {
        let mut line = value.to_string();
        line.push('\n');
        self.writer.try_send(line).is_ok()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub struct Plugin {
    pub manifest: Manifest,
    pub state: RunState,
    pub error: Option<String>,
    pub status: Option<(String, Level)>,
    pub log: VecDeque<String>,
    proc: Option<Proc>,
    /// Пользователь хочет, чтобы плагин работал (включён).
    wanted: bool,
    restarts: u32,
    restart_at: Option<Instant>,
    user_config: Map<String, Value>,
}

impl Plugin {
    fn new(manifest: Manifest) -> Self {
        Self {
            manifest,
            state: RunState::Stopped,
            error: None,
            status: None,
            log: VecDeque::new(),
            proc: None,
            wanted: false,
            restarts: 0,
            restart_at: None,
            user_config: Map::new(),
        }
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_KEEP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn config(&self) -> Map<String, Value> {
        self.manifest.effective_config(&self.user_config)
    }
}

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn spawn(
    manifest: &Manifest,
    config: &Map<String, Value>,
    gen: u64,
    notify: &Notify,
) -> Result<Proc, String> {
    let data_dir = crate::model::plugin_data_dir(&manifest.id);
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("data folder: {e}"))?;

    let program = manifest.program();
    let mut cmd = Command::new(&program);
    cmd.args(&manifest.run.command[1..])
        .current_dir(&manifest.dir)
        .env("MPT_API", API_VERSION.to_string())
        .env("MPT_PLUGIN_ID", &manifest.id)
        .env("MPT_PLUGIN_DIR", &manifest.dir)
        .env("MPT_DATA_DIR", &data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: без мигающей консоли.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("failed to start '{program}': {e}"))?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("failed to attach process pipes".into());
    };

    let (writer, queue) = mpsc::sync_channel::<String>(WRITE_QUEUE);
    std::thread::spawn(move || {
        let mut stdin = stdin;
        while let Ok(line) = queue.recv() {
            if stdin.write_all(line.as_bytes()).is_err() || stdin.flush().is_err() {
                break;
            }
        }
    });

    let id = manifest.id.clone();
    let notify_out = notify.clone();
    std::thread::spawn(move || read_stdout(stdout, &id, gen, &notify_out));

    let id = manifest.id.clone();
    let notify_err = notify.clone();
    std::thread::spawn(move || read_stderr(stderr, &id, gen, &notify_err));

    let proc = Proc {
        child,
        writer,
        gen,
        started: Instant::now(),
        ready: false,
        subscribed: HashSet::new(),
        calls: VecDeque::new(),
        stopping: None,
    };
    proc.send(&json!({
        "type": "hello",
        "api": API_VERSION,
        "core_version": env!("CARGO_PKG_VERSION"),
        "plugin_id": manifest.id,
        "data_dir": data_dir.to_string_lossy(),
        "config": Value::Object(config.clone()),
        "permissions": manifest.permissions,
    }));
    Ok(proc)
}

fn read_stdout(stdout: std::process::ChildStdout, id: &str, gen: u64, notify: &Notify) {
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::new();
    let emit = |kind: IncomingKind| {
        notify(Incoming {
            id: id.to_string(),
            gen,
            kind,
        })
    };
    loop {
        buf.clear();
        let n = match (&mut reader)
            .take(MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut buf)
        {
            Ok(n) => n,
            Err(_) => return,
        };
        if n == 0 {
            return;
        }
        if buf.last() != Some(&b'\n') && buf.len() > MAX_LINE {
            // Строка длиннее 1 МиБ: выбрасываем её остаток до перевода строки.
            loop {
                buf.clear();
                match (&mut reader).take(64 * 1024).read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if buf.last() == Some(&b'\n') {
                            break;
                        }
                    }
                }
            }
            emit(IncomingKind::Log("line longer than 1 MiB dropped".into()));
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(text) {
            Ok(value) if value.is_object() => emit(IncomingKind::Msg(value)),
            _ => emit(IncomingKind::Log(format!(
                "stdout is not a JSON message: {}",
                clip(text, 120)
            ))),
        }
    }
}

fn read_stderr(stderr: std::process::ChildStderr, id: &str, gen: u64, notify: &Notify) {
    let mut reader = BufReader::new(stderr);
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        match (&mut reader).take(8192).read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf);
                let text = text.trim_end();
                if !text.is_empty() {
                    notify(Incoming {
                        id: id.to_string(),
                        gen,
                        kind: IncomingKind::Log(format!("stderr: {text}")),
                    });
                }
            }
        }
    }
}

// ───────────────────────────── хост ─────────────────────────────

pub struct Host {
    pub plugins: Vec<Plugin>,
    /// Папки с плагинами, чей манифест не прошёл проверку: (папка, причина).
    pub invalid: Vec<(String, String)>,
    /// Растёт, когда очередной плагин сообщил `ready`: по нему ядро понимает,
    /// что новичку нужно прислать текущее состояние плеера.
    pub ready_gen: u64,
    notify: Notify,
    next_gen: u64,
}

fn scan_dir() -> (Vec<Manifest>, Vec<(String, String)>) {
    let mut found = Vec::new();
    let mut invalid = Vec::new();
    let Ok(entries) = std::fs::read_dir(crate::model::plugins_dir()) else {
        return (found, invalid);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let folder = entry.file_name().to_string_lossy().to_string();
        // Служебные папки (например, «<id>.old» после обновления) пропускаем.
        if folder.starts_with('.') || folder.ends_with(".old") || folder.ends_with(".tmp") {
            continue;
        }
        if !path.join("plugin.json").is_file() {
            continue;
        }
        match Manifest::load(&path) {
            Ok(manifest) => found.push(manifest),
            Err(reason) => invalid.push((folder, reason)),
        }
    }
    found.sort_by_cached_key(|m| m.name.to_lowercase());
    invalid.sort();
    (found, invalid)
}

impl Host {
    pub fn scan(notify: Notify) -> Host {
        let (found, invalid) = scan_dir();
        Host {
            plugins: found.into_iter().map(Plugin::new).collect(),
            invalid,
            ready_gen: 0,
            notify,
            next_gen: 1,
        }
    }

    /// Перечитывает папку плагинов. Работающие плагины остаются как есть.
    pub fn rescan(&mut self) {
        let (found, invalid) = scan_dir();
        let mut old = std::mem::take(&mut self.plugins);
        for manifest in found {
            match old.iter().position(|p| p.manifest.id == manifest.id) {
                Some(pos) => {
                    let mut plugin = old.remove(pos);
                    if plugin.proc.is_none() {
                        plugin.manifest = manifest;
                    }
                    self.plugins.push(plugin);
                }
                None => self.plugins.push(Plugin::new(manifest)),
            }
        }
        // Оставшиеся в `old` удалены с диска; Drop процесса остановит их.
        self.invalid = invalid;
    }

    pub fn position(&self, id: &str) -> Option<usize> {
        self.plugins.iter().position(|p| p.manifest.id == id)
    }

    pub fn has_perm(&self, idx: usize, perm: &str) -> bool {
        self.plugins
            .get(idx)
            .map(|p| p.manifest.permissions.iter().any(|x| x == perm))
            .unwrap_or(false)
    }

    /// Есть ли хоть один плагин, готовый получать события.
    pub fn any_ready(&self) -> bool {
        self.plugins.iter().any(|p| {
            p.proc
                .as_ref()
                .map(|x| x.ready && x.stopping.is_none())
                .unwrap_or(false)
        })
    }

    /// Включает плагин: запускает процесс (или планирует запуск, если старый ещё завершается).
    pub fn start(&mut self, idx: usize, user_config: &Map<String, Value>) {
        let Host {
            plugins,
            notify,
            next_gen,
            ..
        } = self;
        let Some(plugin) = plugins.get_mut(idx) else {
            return;
        };
        plugin.wanted = true;
        plugin.restarts = 0;
        plugin.restart_at = None;
        plugin.error = None;
        plugin.user_config = user_config.clone();
        if plugin.proc.is_none() {
            let gen = *next_gen;
            *next_gen += 1;
            launch(plugin, gen, notify);
        }
    }

    /// Выключает плагин: `shutdown`, через 3 секунды — kill (см. `tick`).
    pub fn stop(&mut self, idx: usize) {
        let Some(plugin) = self.plugins.get_mut(idx) else {
            return;
        };
        plugin.wanted = false;
        plugin.restart_at = None;
        plugin.error = None;
        match plugin.proc.as_mut() {
            Some(proc) => {
                if proc.stopping.is_none() {
                    proc.send(&json!({"type": "shutdown"}));
                    proc.stopping = Some(Instant::now());
                }
                plugin.state = RunState::Stopping;
            }
            None => plugin.state = RunState::Stopped,
        }
    }

    /// Вызывать каждый кадр. Возвращает строки для строки статуса.
    pub fn tick(&mut self) -> Vec<String> {
        let Host {
            plugins,
            notify,
            next_gen,
            ..
        } = self;
        let now = Instant::now();
        let mut notes = Vec::new();
        for plugin in plugins.iter_mut() {
            let mut exited: Option<String> = None;
            let mut intentional = false;
            let mut ran_long = false;
            if let Some(proc) = plugin.proc.as_mut() {
                match proc.child.try_wait() {
                    Ok(Some(code)) => {
                        exited = Some(code.to_string());
                        intentional = proc.stopping.is_some();
                        ran_long = proc.started.elapsed() > STABLE_RUN;
                    }
                    Ok(None) => {
                        if let Some(since) = proc.stopping {
                            if since.elapsed() > SHUTDOWN_TIMEOUT {
                                let _ = proc.child.kill();
                                plugin.push_log("did not exit within 3 s, killed".into());
                            }
                        } else if !proc.ready && proc.started.elapsed() > READY_TIMEOUT {
                            let _ = proc.child.kill();
                            plugin.error = Some("did not send ready within 5 s".into());
                        }
                    }
                    Err(_) => {}
                }
            }
            if let Some(code) = exited {
                plugin.proc = None;
                if intentional {
                    plugin.state = RunState::Stopped;
                } else {
                    if ran_long {
                        plugin.restarts = 0;
                    }
                    let reason = plugin
                        .error
                        .take()
                        .unwrap_or_else(|| format!("process exited ({code})"));
                    let name = plugin.manifest.name.clone();
                    if (plugin.restarts as usize) < RESTART_DELAYS.len() {
                        let delay = RESTART_DELAYS[plugin.restarts as usize];
                        plugin.restarts += 1;
                        plugin.restart_at = Some(now + Duration::from_secs(delay));
                        plugin.state = RunState::Restarting;
                        plugin.error = Some(reason.clone());
                        notes.push(format!("[{name}] {reason}; restarting in {delay} s"));
                    } else {
                        plugin.state = RunState::Failed;
                        plugin.wanted = false;
                        plugin.error = Some(format!("{reason}; restarts exhausted"));
                        notes.push(format!("[{name}] disabled: {reason}"));
                    }
                }
            }
            let due = plugin.restart_at.map(|t| now >= t).unwrap_or(false);
            let idle_wanted =
                plugin.wanted && plugin.proc.is_none() && matches!(plugin.state, RunState::Stopped);
            if plugin.proc.is_none() && plugin.wanted && (due || idle_wanted) {
                plugin.restart_at = None;
                let gen = *next_gen;
                *next_gen += 1;
                launch(plugin, gen, notify);
            }
        }
        notes
    }

    /// Разбирает сообщение от читателя потока.
    pub fn handle(&mut self, incoming: Incoming) -> Handled {
        let Some(idx) = self.position(&incoming.id) else {
            return Handled::Nothing;
        };
        let plugin = &mut self.plugins[idx];
        if plugin.proc.as_ref().map(|p| p.gen) != Some(incoming.gen) {
            return Handled::Nothing;
        }
        let msg = match incoming.kind {
            IncomingKind::Log(line) => {
                plugin.push_log(line);
                return Handled::Nothing;
            }
            IncomingKind::Msg(msg) => msg,
        };
        let kind = msg.get("type").and_then(Value::as_str).unwrap_or("");
        let Some(proc) = plugin.proc.as_mut() else {
            return Handled::Nothing;
        };

        if kind == "ready" {
            if !proc.ready {
                proc.ready = true;
                self.ready_gen += 1;
                proc.subscribed = msg
                    .get("subscribe")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .filter(|s| EVENTS.contains(s))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if proc.stopping.is_none() {
                    plugin.state = RunState::Running;
                }
                plugin.error = None;
            }
            return Handled::Nothing;
        }
        if !proc.ready {
            plugin.push_log(format!("'{kind}' before ready ignored"));
            return Handled::Nothing;
        }

        match kind {
            "request" => {
                let now = Instant::now();
                while proc
                    .calls
                    .front()
                    .map(|t| now.duration_since(*t) > Duration::from_secs(1))
                    .unwrap_or(false)
                {
                    proc.calls.pop_front();
                }
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                if id.is_null() {
                    plugin.push_log("request without id ignored".into());
                    return Handled::Nothing;
                }
                if proc.calls.len() >= MAX_REQUESTS_PER_SEC {
                    proc.send(&error_response(
                        &id,
                        "busy",
                        "at most 50 requests per second",
                    ));
                    return Handled::Nothing;
                }
                proc.calls.push_back(now);
                let Some(method) = msg.get("method").and_then(Value::as_str) else {
                    proc.send(&error_response(
                        &id,
                        "invalid_params",
                        "missing field method",
                    ));
                    return Handled::Nothing;
                };
                Handled::Request(Request {
                    plugin: idx,
                    id,
                    method: method.to_string(),
                    params: msg.get("params").cloned().unwrap_or(Value::Null),
                })
            }
            "status" => {
                let text = clip(msg.get("text").and_then(Value::as_str).unwrap_or(""), 200);
                let level = Level::parse(msg.get("level").and_then(Value::as_str));
                let line = format!("[{}] {text}", plugin.manifest.name);
                plugin.status = Some((text, level));
                Handled::Status(line)
            }
            "log" => {
                let text = clip(msg.get("msg").and_then(Value::as_str).unwrap_or(""), 500);
                plugin.push_log(text);
                Handled::Nothing
            }
            _ => Handled::Nothing,
        }
    }

    pub fn respond(&mut self, idx: usize, id: Value, result: ApiResult) {
        let Some(proc) = self.plugins.get(idx).and_then(|p| p.proc.as_ref()) else {
            return;
        };
        let value = match result {
            Ok(result) => json!({"type": "response", "id": id, "ok": true, "result": result}),
            Err((code, message)) => error_response(&id, code, &message),
        };
        proc.send(&value);
    }

    /// Рассылает событие подписанным плагинам, у которых есть нужное право.
    pub fn broadcast(&self, event: &str, data: Value) {
        let perm = event_permission(event);
        let msg = json!({"type": "event", "name": event, "data": data});
        for plugin in &self.plugins {
            let Some(proc) = plugin.proc.as_ref() else {
                continue;
            };
            if !proc.ready || proc.stopping.is_some() || !proc.subscribed.contains(event) {
                continue;
            }
            if let Some(perm) = perm {
                if !plugin.manifest.permissions.iter().any(|p| p == perm) {
                    continue;
                }
            }
            proc.send(&msg);
        }
    }

    /// Новые настройки: `config_changed` приходит одному плагину всегда, без подписки.
    pub fn update_config(&mut self, idx: usize, user_config: &Map<String, Value>) {
        let Some(plugin) = self.plugins.get_mut(idx) else {
            return;
        };
        plugin.user_config = user_config.clone();
        let config = plugin.config();
        if let Some(proc) = plugin.proc.as_ref() {
            if proc.ready && proc.stopping.is_none() {
                proc.send(&json!({
                    "type": "event",
                    "name": "config_changed",
                    "data": {"config": Value::Object(config)},
                }));
            }
        }
    }

    /// Кнопка из `commands`. Возвращает false, если плагин не готов или команды нет.
    pub fn send_command(&self, idx: usize, name: &str) -> bool {
        let Some(plugin) = self.plugins.get(idx) else {
            return false;
        };
        if !plugin.manifest.commands.iter().any(|c| c.name == name) {
            return false;
        }
        match plugin.proc.as_ref() {
            Some(proc) if proc.ready && proc.stopping.is_none() => {
                proc.send(&json!({"type": "command", "name": name, "args": {}}))
            }
            _ => false,
        }
    }

    /// Останавливает все плагины: `shutdown`, ожидание до 3 секунд, затем kill.
    pub fn shutdown_all(&mut self) {
        for plugin in &mut self.plugins {
            plugin.wanted = false;
            if let Some(proc) = plugin.proc.as_mut() {
                if proc.stopping.is_none() {
                    proc.send(&json!({"type": "shutdown"}));
                    proc.stopping = Some(Instant::now());
                }
            }
        }
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        loop {
            let mut alive = false;
            for plugin in &mut self.plugins {
                if let Some(proc) = plugin.proc.as_mut() {
                    if matches!(proc.child.try_wait(), Ok(None)) {
                        alive = true;
                    }
                }
            }
            if !alive || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // Drop процесса добивает то, что не успело выйти.
        for plugin in &mut self.plugins {
            plugin.proc = None;
            plugin.state = RunState::Stopped;
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

fn launch(plugin: &mut Plugin, gen: u64, notify: &Notify) {
    let config = plugin.config();
    match spawn(&plugin.manifest, &config, gen, notify) {
        Ok(proc) => {
            plugin.proc = Some(proc);
            plugin.state = RunState::Starting;
            plugin.error = None;
        }
        Err(reason) => {
            plugin.state = RunState::Failed;
            plugin.wanted = false;
            plugin.error = Some(reason);
        }
    }
}

fn error_response(id: &Value, code: &str, message: &str) -> Value {
    json!({
        "type": "response",
        "id": id,
        "ok": false,
        "error": {"code": code, "message": message},
    })
}
