//! Lightweight programme level control for contribution streams.
//!
//! This is deliberately a low-CPU RMS leveler, not an EBU R128 measurement
//! engine. It avoids FFTs, true-peak oversampling and look-ahead buffering so
//! it remains appropriate for a live 48 kHz contribution path.

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    pub enabled: bool,
    /// Programme target expressed as RMS dBFS. -16 dBFS is a conservative
    /// live-streaming default; this is not interchangeable with integrated LUFS.
    pub target_dbfs: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            target_dbfs: -16.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub short_term_dbfs: f32,
    pub gain_db: f32,
    pub limiting: bool,
}

pub struct Controller {
    settings: Settings,
    gain: f32,
    snapshot: Snapshot,
}

impl Controller {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            gain: 1.0,
            snapshot: Snapshot {
                short_term_dbfs: -96.0,
                ..Snapshot::default()
            },
        }
    }

    pub fn settings(&self) -> Settings {
        self.settings
    }

    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = settings;
        if !settings.enabled {
            self.gain = 1.0;
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
    }

    /// Processes interleaved PCM on the routing worker. `frames` must be the
    /// number of stereo frames represented by `samples`.
    pub fn process_stereo(&mut self, samples: &mut [f32], frames: usize) {
        if frames == 0 {
            return;
        }
        let rms = (samples
            .iter()
            .map(|sample| f64::from(*sample) * f64::from(*sample))
            .sum::<f64>()
            / samples.len().max(1) as f64)
            .sqrt() as f32;
        let measured = if rms > 0.0 {
            (20.0 * rms.log10()).max(-96.0)
        } else {
            -96.0
        };
        if self.settings.enabled && measured > -60.0 {
            let target_gain =
                10_f32.powf((self.settings.target_dbfs - measured).clamp(-18.0, 12.0) / 20.0);
            // Gain rises slowly (about 1.5 s) and falls faster (about 80 ms),
            // which avoids pumping while protecting sudden loud changes.
            let coefficient = if target_gain < self.gain { 0.20 } else { 0.006 };
            self.gain += (target_gain - self.gain) * coefficient * (frames as f32 / 480.0).min(1.0);
        } else if !self.settings.enabled {
            self.gain = 1.0;
        }
        let mut limiting = false;
        for sample in samples {
            let controlled = *sample * self.gain;
            if controlled.abs() > 0.988_553_1 {
                limiting = true;
            }
            *sample = controlled.clamp(-0.988_553_1, 0.988_553_1);
        }
        self.snapshot = Snapshot {
            short_term_dbfs: measured,
            gain_db: if self.gain > 0.0 {
                20.0 * self.gain.log10()
            } else {
                -96.0
            },
            limiting,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_never_exceeds_the_safety_ceiling() {
        let mut leveler = Controller::new(Settings::default());
        let mut audio = vec![2.0; 960];
        leveler.process_stereo(&mut audio, 480);
        assert!(audio.iter().all(|sample| sample.abs() <= 0.988_553_1));
        assert!(leveler.snapshot().limiting);
    }
}
