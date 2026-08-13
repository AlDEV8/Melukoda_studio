//! Platform-independent operational core.  Audio capture and the supervised
//! encoder live at the shell boundary; its decisions and persisted state live here.
pub mod diagnostics;
pub mod loudness;
pub mod profile;
pub mod recording;
pub mod reliability;
pub mod spool;

pub const INTERNAL_SAMPLE_RATE: u32 = 48_000;
pub const INTERNAL_CHANNELS: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StereoFrame {
    pub left: f32,
    pub right: f32,
}

pub fn map_input(frame: &[f32], mono_mix: bool) -> StereoFrame {
    let left = frame.first().copied().unwrap_or_default();
    let right = frame.get(1).copied().unwrap_or(left);
    if mono_mix {
        let m = (left + right) * 0.5;
        StereoFrame { left: m, right: m }
    } else {
        StereoFrame { left, right }
    }
}

/// Transparent, instant-attack safety limiter.  It never amplifies samples.
pub fn limit(sample: f32, enabled: bool) -> f32 {
    if !enabled {
        return sample;
    }
    sample.clamp(-0.988_553_1, 0.988_553_1) // -0.1 dBFS
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mono_input_is_dual_mono() {
        assert_eq!(
            map_input(&[0.2], false),
            StereoFrame {
                left: 0.2,
                right: 0.2
            }
        );
    }
    #[test]
    fn mix_is_equal_powerless_average() {
        assert_eq!(
            map_input(&[1., -1.], true),
            StereoFrame {
                left: 0.,
                right: 0.
            }
        );
    }
    #[test]
    fn limiter_preserves_safe_signal() {
        assert_eq!(limit(0.2, true), 0.2);
        assert!(limit(2., true) < 1.);
    }
}
