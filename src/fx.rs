//! Обработка звука: 10-полосный эквалайзер и панорама.
//!
//! Параметры лежат в общей структуре, поэтому ползунки в настройках меняют
//! звук на лету, не перезапуская трек.

use rodio::source::SeekError;
use rodio::Source;
use std::sync::{Arc, Mutex};

use crate::settings::EQ_BANDS;

#[derive(Clone, Copy, Debug, Default)]
pub struct FxParams {
    pub enabled: bool,
    pub gains: [f32; EQ_BANDS],
    pub pan: f32,
}

pub type SharedFx = Arc<Mutex<FxParams>>;

pub fn shared(params: FxParams) -> SharedFx {
    Arc::new(Mutex::new(params))
}

/// Полосовой пиковый фильтр (RBJ cookbook), трансформированная прямая форма II.
#[derive(Clone, Copy, Default)]
struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn peaking(sample_rate: f32, freq: f32, gain_db: f32) -> Self {
        if gain_db.abs() < 0.05 || freq >= sample_rate * 0.45 {
            return Self {
                b0: 1.0,
                ..Default::default()
            };
        }
        let amp = 10f32.powf(gain_db / 40.0);
        let w0 = 2.0 * std::f32::consts::PI * freq / sample_rate;
        let (sin, cos) = w0.sin_cos();
        let alpha = sin / (2.0 * 1.1);
        let a0 = 1.0 + alpha / amp;
        Self {
            b0: (1.0 + alpha * amp) / a0,
            b1: (-2.0 * cos) / a0,
            b2: (1.0 - alpha * amp) / a0,
            a1: (-2.0 * cos) / a0,
            a2: (1.0 - alpha / amp) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn is_identity(&self) -> bool {
        self.b0 == 1.0 && self.b1 == 0.0 && self.b2 == 0.0 && self.a1 == 0.0 && self.a2 == 0.0
    }

    /// Меняет коэффициенты, сохраняя состояние линий задержки. Раньше фильтр
    /// пересоздавался с нуля, и каждое движение ползунка давало щелчок.
    fn retune(&mut self, fresh: Biquad) {
        if fresh.is_identity() {
            *self = fresh;
        } else {
            self.b0 = fresh.b0;
            self.b1 = fresh.b1;
            self.b2 = fresh.b2;
            self.a1 = fresh.a1;
            self.a2 = fresh.a2;
        }
    }

    fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

pub struct FxSource<S> {
    inner: S,
    shared: SharedFx,
    channels: u16,
    sample_rate: u32,
    filters: Vec<[Biquad; EQ_BANDS]>,
    local: FxParams,
    refresh_countdown: u32,
    frame: u16,
    pan_left: f32,
    pan_right: f32,
}

impl<S> FxSource<S>
where
    S: Source<Item = i16>,
{
    pub fn new(inner: S, shared: SharedFx) -> Self {
        let channels = inner.channels().max(1);
        let sample_rate = inner.sample_rate().max(8000);
        let local = shared.lock().map(|p| *p).unwrap_or_default();
        let mut source = Self {
            inner,
            shared,
            channels,
            sample_rate,
            filters: vec![[Biquad::default(); EQ_BANDS]; channels as usize],
            local,
            refresh_countdown: 0,
            frame: 0,
            pan_left: 1.0,
            pan_right: 1.0,
        };
        source.rebuild();
        source
    }

    fn rebuild(&mut self) {
        let gains = self.local.gains;
        let sr = self.sample_rate as f32;
        for channel in self.filters.iter_mut() {
            for (band, filter) in channel.iter_mut().enumerate() {
                let gain = if self.local.enabled { gains[band] } else { 0.0 };
                filter.retune(Biquad::peaking(sr, crate::settings::EQ_FREQS[band], gain));
            }
        }
        let pan = self.local.pan;
        self.pan_left = if pan > 0.0 { 1.0 - pan } else { 1.0 };
        self.pan_right = if pan < 0.0 { 1.0 + pan } else { 1.0 };
    }

    fn maybe_refresh(&mut self) {
        if self.refresh_countdown > 0 {
            self.refresh_countdown -= 1;
            return;
        }
        self.refresh_countdown = 63;
        let current = self.shared.lock().map(|p| *p).unwrap_or_default();
        if current.enabled != self.local.enabled
            || current.pan != self.local.pan
            || current.gains != self.local.gains
        {
            self.local = current;
            self.rebuild();
        }
    }

    fn bypass(&self) -> bool {
        !self.local.enabled && self.local.pan.abs() < 0.001
    }
}

impl<S> Iterator for FxSource<S>
where
    S: Source<Item = i16>,
{
    type Item = i16;

    fn next(&mut self) -> Option<i16> {
        let sample = self.inner.next()?;
        self.maybe_refresh();
        // Счётчик канала двигаем всегда, в том числе в обходе. Для стерео раньше
        // совпадало случайно (опрос параметров каждые 64 сэмпла — чётное число),
        // а для 3+ каналов состояние фильтров «съезжало» между каналами.
        let channel = self.frame as usize;
        self.frame = (self.frame + 1) % self.channels;
        if self.bypass() {
            return Some(sample);
        }

        let mut value = sample as f32 / 32768.0;
        if self.local.enabled {
            if let Some(filters) = self.filters.get_mut(channel) {
                for filter in filters.iter_mut() {
                    value = filter.process(value);
                }
            }
        }
        if self.channels == 2 {
            value *= if channel == 0 {
                self.pan_left
            } else {
                self.pan_right
            };
        } else {
            value *= (self.pan_left + self.pan_right) * 0.5;
        }
        Some((value.clamp(-1.0, 1.0) * 32767.0) as i16)
    }
}

impl<S> Source for FxSource<S>
where
    S: Source<Item = i16>,
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

    fn total_duration(&self) -> Option<std::time::Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, pos: std::time::Duration) -> Result<(), SeekError> {
        self.inner.try_seek(pos)?;
        self.frame = 0;
        for channel in self.filters.iter_mut() {
            for filter in channel.iter_mut() {
                filter.reset();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RMS синуса 1 кГц после полосового фильтра.
    fn rms(gain_db: f32) -> f32 {
        let sr = 44100.0f32;
        let mut filter = Biquad::peaking(sr, 1000.0, gain_db);
        let mut sum = 0.0;
        let total = 24_000;
        for i in 0..total {
            let phase = 2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sr;
            let sample = phase.sin() * 0.5;
            let out = filter.process(sample);
            if i > 4_000 {
                sum += out * out;
            }
        }
        (sum / (total - 4_000) as f32).sqrt()
    }

    #[test]
    fn eq_boost_and_cut_change_level() {
        let flat = rms(0.0);
        let boosted = rms(9.0);
        let cut = rms(-9.0);
        assert!(boosted > flat * 1.8, "boost {boosted} vs {flat}");
        assert!(cut < flat * 0.62, "cut {cut} vs {flat}");
    }

    #[test]
    fn negative_gain_has_no_nan() {
        let value = rms(-12.0);
        assert!(value.is_finite() && value >= 0.0);
    }
}
