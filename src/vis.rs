//! Спектр для визуализатора: берём последние сэмплы и считаем FFT.

pub const FFT_N: usize = 2048;
pub const BANDS: usize = 56;

/// Возвращает BANDS значений 0..1 (log-шкала частот).
pub fn bands(samples: &[f32]) -> Vec<f32> {
    let mut re = vec![0.0f32; FFT_N];
    let mut im = vec![0.0f32; FFT_N];

    let take = samples.len().min(FFT_N);
    if take == 0 {
        return vec![0.0; BANDS];
    }
    let offset = FFT_N - take;
    let start = samples.len() - take;
    for i in 0..take {
        let w = 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / take as f32).cos();
        re[offset + i] = samples[start + i] * w;
    }

    fft(&mut re, &mut im);

    let half = FFT_N / 2;
    let mut mags = vec![0.0f32; half];
    for i in 0..half {
        mags[i] = (re[i] * re[i] + im[i] * im[i]).sqrt();
    }

    let rate = 44_100.0f32;
    let mut out = vec![0.0f32; BANDS];
    for (b, value) in out.iter_mut().enumerate() {
        let f0 = 40.0 * (12_000.0f32 / 40.0).powf(b as f32 / BANDS as f32);
        let f1 = 40.0 * (12_000.0f32 / 40.0).powf((b + 1) as f32 / BANDS as f32);
        let i0 = ((f0 / rate) * FFT_N as f32) as usize;
        let i1 = (((f1 / rate) * FFT_N as f32) as usize).max(i0 + 1).min(half);
        let i0 = i0.min(half - 1);
        let peak = mags[i0..i1].iter().copied().fold(0.0f32, f32::max);
        // Примерная громкость в дБ относительно полной шкалы.
        let db = 20.0 * (peak / 64.0 + 1e-7).log10();
        *value = ((db + 62.0) / 62.0).clamp(0.0, 1.0);
    }
    out
}

/// Итеративное radix-2 FFT на месте.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());

    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2;
    while len <= n {
        let angle = -std::f32::consts::TAU / len as f32;
        let (wr, wi) = (angle.cos(), angle.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let (ur, ui) = (re[i + k], im[i + k]);
                let (vr, vi) = (
                    re[i + k + len / 2] * cr - im[i + k + len / 2] * ci,
                    re[i + k + len / 2] * ci + im[i + k + len / 2] * cr,
                );
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + len / 2] = ur - vr;
                im[i + k + len / 2] = ui - vi;
                let next_r = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = next_r;
            }
            i += len;
        }
        len <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_has_one_loud_band() {
        let rate = 44_100.0f32;
        let freq = 440.0f32;
        let samples: Vec<f32> = (0..FFT_N)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / rate).sin() * 0.9)
            .collect();
        let values = bands(&samples);
        let max = values.iter().cloned().fold(0.0f32, f32::max);
        let loud = values.iter().filter(|v| **v > max * 0.8).count();
        assert!(loud >= 1 && loud <= 3, "loud={loud} max={max}");
        assert!(max > 0.5, "max={max}");
    }

    #[test]
    fn silence_is_zero() {
        let values = bands(&vec![0.0; FFT_N]);
        assert!(values.iter().all(|v| *v == 0.0));
    }
}
