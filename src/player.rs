use crate::model::Track;
use anyhow::{Context, Result};
use rodio::source::SeekError;
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use std::fs::File;
use std::io::BufReader;
use std::time::Duration;

/// Начало сообщения, когда не удалось открыть звуковой выход. Это не вина трека,
/// поэтому `next` в этом случае не перебирает очередь.
const NO_OUTPUT: &str = "Could not open audio output";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    All,
    One,
}

impl RepeatMode {
    pub fn next(self) -> Self {
        match self {
            RepeatMode::Off => RepeatMode::All,
            RepeatMode::All => RepeatMode::One,
            RepeatMode::One => RepeatMode::Off,
        }
    }
}

pub struct Player {
    _stream: OutputStream,
    handle: OutputStreamHandle,
    sink: Option<Sink>,
    pub queue: Vec<Track>,
    /// Растёт при каждой замене очереди (для события `queue_changed` у плагинов).
    pub queue_gen: u64,
    pub index: usize,
    pub playing: bool,
    pub volume: f32,
    pub repeat: RepeatMode,
    pub shuffle: bool,
    pub position: f32,
    /// Поправка к `Sink::get_pos`: позиция, с которой начался текущий sink.
    base: f32,
    pub duration: f32,
    pub error: Option<String>,
    /// Состояние генератора для перемешивания (xorshift64).
    rng: u64,
}

impl Player {
    pub fn new() -> Result<Self> {
        let (stream, handle) = OutputStream::try_default().context("audio output unavailable")?;
        Ok(Self {
            _stream: stream,
            handle,
            sink: None,
            queue: Vec::new(),
            queue_gen: 0,
            index: 0,
            playing: false,
            volume: 0.8,
            repeat: RepeatMode::Off,
            shuffle: false,
            position: 0.0,
            base: 0.0,
            duration: 0.0,
            error: None,
            rng: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15)
                | 1,
        })
    }

    fn random(&mut self) -> usize {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x as usize
    }

    pub fn current(&self) -> Option<&Track> {
        self.queue.get(self.index)
    }

    pub fn play_queue(&mut self, tracks: Vec<Track>, start: usize) {
        if tracks.is_empty() {
            return;
        }
        self.queue = tracks;
        self.queue_gen += 1;
        self.index = start.min(self.queue.len() - 1);
        self.start_current();
    }

    fn start_current(&mut self) {
        self.start_at(0.0);
    }

    /// Запускает текущий трек с позиции `from` (секунды). Ненулевая позиция
    /// нужна для перемотки, когда декодер не умеет `try_seek`: файл открывается
    /// заново, начало пропускается.
    fn start_at(&mut self, from: f32) {
        self.error = None;
        // Пока не запустили — считаем, что не играем (иначе при ошибке UI «играет» в тишине).
        self.playing = false;
        let Some(track) = self.queue.get(self.index) else {
            return;
        };
        self.duration = track.duration_ms as f32 / 1000.0;
        self.position = from;
        self.base = from;

        // Старый sink останавливаем, создаём новый — так надёжнее, чем clear().
        self.sink = None;

        let sink = match Sink::try_new(&self.handle) {
            Ok(s) => s,
            Err(e) => {
                self.error = Some(format!("{NO_OUTPUT}: {e}"));
                return;
            }
        };
        let file = match File::open(&track.path) {
            Ok(f) => f,
            Err(e) => {
                self.error = Some(format!("Cannot open file: {e}"));
                return;
            }
        };
        match Decoder::new(BufReader::new(file)) {
            Ok(decoder) => {
                // Начало пропускается лениво, в аудиопотоке (см. SkipSource):
                // раньше skip_duration декодировал всё до нужной секунды прямо
                // в потоке интерфейса, и окно «не отвечало».
                let source = SkipSource::new(decoder, Duration::from_secs_f32(from));
                // Громкость — до `append`: иначе первые сэмплы могут прозвучать на полной.
                sink.set_volume(self.volume);
                sink.append(source);
                sink.play();
                self.playing = true;
                self.sink = Some(sink);
            }
            Err(e) => {
                self.error = Some(format!("Unsupported format: {e}"));
            }
        }
    }

    pub fn toggle(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        match &self.sink {
            Some(sink) => {
                if sink.empty() {
                    // Очередь доиграла до конца: PLAY запускает трек заново
                    // (раньше кнопка при этом ничего не делала).
                    self.start_current();
                } else if sink.is_paused() {
                    sink.play();
                    self.playing = true;
                } else {
                    sink.pause();
                    self.playing = false;
                }
            }
            None => self.start_current(),
        }
    }

    /// Переходит к следующему индексу очереди по правилам перемешивания и повтора.
    /// `false` — очередь закончилась.
    fn step_forward(&mut self) -> bool {
        if self.shuffle && self.queue.len() > 1 {
            let mut idx = self.random() % self.queue.len();
            if idx == self.index {
                idx = (idx + 1) % self.queue.len();
            }
            self.index = idx;
        } else if self.index + 1 < self.queue.len() {
            self.index += 1;
        } else if self.repeat != RepeatMode::Off {
            self.index = 0;
        } else {
            return false;
        }
        true
    }

    pub fn next(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if !self.step_forward() {
            self.playing = false;
            if let Some(sink) = &self.sink {
                sink.pause();
            }
            return;
        }
        self.start_current();
        // Пропавший или битый файл не должен останавливать очередь:
        // пробуем следующие, но не больше одного круга.
        let mut skipped: Option<String> = None;
        for _ in 1..self.queue.len() {
            if self.playing {
                break;
            }
            let Some(reason) = self.error.clone().filter(|e| !e.starts_with(NO_OUTPUT)) else {
                break;
            };
            let title = self
                .queue
                .get(self.index)
                .map_or("track", |t| t.title.as_str());
            skipped.get_or_insert_with(|| format!("Skipped “{title}”: {reason}"));
            if !self.step_forward() {
                break;
            }
            self.start_current();
        }
        if self.playing && skipped.is_some() {
            // Очередь играет дальше, но о пропуске стоит сказать.
            self.error = skipped;
        }
    }

    pub fn prev(&mut self) {
        if self.queue.is_empty() {
            return;
        }
        if self.position > 4.0 {
            self.seek(0.0);
            return;
        }
        if self.index > 0 {
            self.index -= 1;
        } else if self.repeat == RepeatMode::All {
            self.index = self.queue.len() - 1;
        }
        self.start_current();
    }

    pub fn seek(&mut self, secs: f32) {
        let mut target = secs.max(0.0);
        if self.duration > 1.0 {
            target = target.min(self.duration - 0.5);
        }
        let was_playing = self.playing;
        let Some(sink) = &self.sink else { return };
        match sink.try_seek(Duration::from_secs_f32(target)) {
            Ok(()) => {
                // Что бы ни показывал get_pos после перемотки, дальше считаем от target.
                self.base = target - sink.get_pos().as_secs_f32();
                self.position = target;
            }
            Err(_) => {
                // Декодер не умеет перематывать (например claxon для FLAC):
                // открываем файл заново и пропускаем начало.
                self.start_at(target);
                if !was_playing {
                    if let Some(sink) = &self.sink {
                        sink.pause();
                    }
                    self.playing = false;
                }
            }
        }
    }

    pub fn set_volume(&mut self, v: f32) {
        self.volume = v.clamp(0.0, 1.0);
        if let Some(sink) = &self.sink {
            sink.set_volume(self.volume);
        }
    }

    /// Вызывать регулярно: обновляет позицию и, когда трек доиграл, переходит к следующему.
    pub fn tick(&mut self) {
        let Some(sink) = &self.sink else { return };
        if self.playing && sink.empty() {
            if self.repeat == RepeatMode::One {
                self.start_current();
                return;
            }
            let last = self.index + 1 >= self.queue.len();
            if last && self.repeat == RepeatMode::Off && !self.shuffle {
                self.playing = false;
                self.position = self.duration;
                return;
            }
            self.next();
            return;
        }
        if !sink.empty() {
            let pos = self.base + sink.get_pos().as_secs_f32();
            self.position = if self.duration > 0.0 {
                pos.min(self.duration)
            } else {
                pos
            };
        }
    }
}

/// Обёртка над источником, которая лениво пропускает начало в аудиопотоке
/// (перемотка для декодеров без `try_seek`) и пробрасывает `try_seek` дальше.
struct SkipSource<S> {
    inner: S,
    /// Сколько сэмплов выбросить с начала.
    skip: usize,
}

impl<S> SkipSource<S>
where
    S: rodio::Source<Item = i16>,
{
    fn new(inner: S, from: Duration) -> Self {
        let channels = inner.channels().max(1) as usize;
        let frames = (from.as_secs_f64() * inner.sample_rate() as f64) as usize;
        Self {
            inner,
            skip: frames * channels,
        }
    }
}

impl<S> Iterator for SkipSource<S>
where
    S: rodio::Source<Item = i16>,
{
    type Item = i16;

    fn next(&mut self) -> Option<i16> {
        while self.skip > 0 {
            self.inner.next()?;
            self.skip -= 1;
        }
        self.inner.next()
    }
}

impl<S> rodio::Source for SkipSource<S>
where
    S: rodio::Source<Item = i16>,
{
    fn current_frame_len(&self) -> Option<usize> {
        self.inner.current_frame_len()
    }

    fn channels(&self) -> u16 {
        self.inner.channels()
    }

    fn sample_rate(&self) -> u32 {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    /// Без этого перемотка не работает: по умолчанию `try_seek` у обёртки
    /// возвращает «не поддерживается», и `Sink::try_seek` молча проваливается.
    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)?;
        self.skip = 0;
        Ok(())
    }
}
