use crate::audio_backend::SinkResult;

use super::wasapi::WasapiSink;
use super::{EffectsConfig, EffectsPreset, effects_snapshot};

/// Compatibility renderer for platforms where XAudio2 can create its default
/// virtual client but rejects explicit render endpoints. Windows 10 Mobile is
/// such a platform. PCM stays in Rust, uses the same user-facing effects, and
/// is delivered to the selected endpoint through the native WASAPI renderer.
pub(super) struct XAudio2EndpointSink {
    output: WasapiSink,
    effects: SoftwareEffects,
}

impl XAudio2EndpointSink {
    pub fn new(sample_rate: u32, channels: u16, device_id: &str) -> SinkResult<Self> {
        Ok(Self {
            output: WasapiSink::new(sample_rate, channels, device_id)?,
            effects: SoftwareEffects::new(sample_rate, channels),
        })
    }

    pub fn start(&mut self) -> SinkResult<()> {
        self.output.start()
    }

    pub fn stop(&mut self) -> SinkResult<()> {
        self.output.stop()
    }

    pub fn flush(&mut self) -> SinkResult<()> {
        self.effects.reset_delay_lines();
        self.output.flush()
    }

    pub fn write_f32(&mut self, samples: &[f32]) -> SinkResult<()> {
        let config = effects_snapshot();
        let processed = self.effects.process(samples, &config);
        self.output.write_f32(&processed)
    }
}

#[derive(Clone, Copy, Debug, Default)]
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
    fn peaking(sample_rate: u32, frequency: f32, gain_db: f32) -> Self {
        let nyquist = sample_rate as f32 * 0.5;
        let frequency = frequency.clamp(20.0, nyquist * 0.95);
        let amplitude = 10.0f32.powf(gain_db / 40.0);
        let omega = std::f32::consts::TAU * frequency / sample_rate as f32;
        let alpha = omega.sin() / (2.0 * 0.85);
        let cosine = omega.cos();
        let a0 = 1.0 + alpha / amplitude;

        Self {
            b0: (1.0 + alpha * amplitude) / a0,
            b1: (-2.0 * cosine) / a0,
            b2: (1.0 - alpha * amplitude) / a0,
            a1: (-2.0 * cosine) / a0,
            a2: (1.0 - alpha / amplitude) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, sample: f32) -> f32 {
        let output = self.b0 * sample + self.z1;
        self.z1 = self.b1 * sample - self.a1 * output + self.z2;
        self.z2 = self.b2 * sample - self.a2 * output;
        output
    }
}

struct SoftwareEffects {
    sample_rate: u32,
    channels: usize,
    applied_version: u64,
    config: EffectsConfig,
    equalizers: Vec<Biquad>,
    echo_line: Vec<f32>,
    echo_cursor: usize,
    reverb_line: Vec<f32>,
    reverb_cursor: usize,
}

impl SoftwareEffects {
    fn new(sample_rate: u32, channels: u16) -> Self {
        Self {
            sample_rate,
            channels: channels as usize,
            applied_version: u64::MAX,
            config: EffectsConfig::default(),
            equalizers: Vec::new(),
            echo_line: Vec::new(),
            echo_cursor: 0,
            reverb_line: Vec::new(),
            reverb_cursor: 0,
        }
    }

    fn process(&mut self, samples: &[f32], config: &EffectsConfig) -> Vec<f32> {
        self.apply_config(config);
        if self.is_passthrough() {
            return samples.to_vec();
        }

        let mut output = Vec::with_capacity(samples.len());
        let gains = effect_gains(&self.config);
        let headroom = effect_headroom(&self.config, &gains);
        let echo_wet = 0.04 + 0.20 * self.config.strength;
        let echo_feedback = 0.05 + 0.25 * self.config.strength;
        let reverb_wet = if self.config.reverb {
            0.08 + 0.14 * self.config.strength
        } else {
            0.0
        };
        let reverb_feedback = 0.18 + 0.24 * self.config.strength;

        for (sample_index, source) in samples.iter().copied().enumerate() {
            let channel = sample_index % self.channels;
            let mut value = if self.equalizers.is_empty() {
                source
            } else {
                let first = channel * gains.len();
                self.equalizers[first..first + gains.len()]
                    .iter_mut()
                    .fold(source, |sample, filter| filter.process(sample))
            };

            if !self.echo_line.is_empty() {
                let delayed = self.echo_line[self.echo_cursor];
                self.echo_line[self.echo_cursor] = value + delayed * echo_feedback;
                value += delayed * echo_wet;
                self.echo_cursor += 1;
                if self.echo_cursor == self.echo_line.len() {
                    self.echo_cursor = 0;
                }
            }

            if !self.reverb_line.is_empty() {
                let delayed = self.reverb_line[self.reverb_cursor];
                self.reverb_line[self.reverb_cursor] = value + delayed * reverb_feedback;
                value += delayed * reverb_wet;
                self.reverb_cursor += 1;
                if self.reverb_cursor == self.reverb_line.len() {
                    self.reverb_cursor = 0;
                }
            }

            value *= headroom;
            if self.config.limiter {
                value = soft_limit(value);
            }
            output.push(if value.is_finite() { value } else { 0.0 });
        }
        output
    }

    fn apply_config(&mut self, config: &EffectsConfig) {
        if config.version == self.applied_version {
            return;
        }

        self.config = config.clone();
        let gains = effect_gains(config);
        self.equalizers.clear();
        if config.preset != EffectsPreset::None {
            let centers = [80.0, 250.0, 1_000.0, 4_000.0, 12_000.0];
            for _ in 0..self.channels {
                self.equalizers.extend(
                    centers.iter().zip(gains.iter()).map(|(frequency, gain)| {
                        Biquad::peaking(self.sample_rate, *frequency, *gain)
                    }),
                );
            }
        }

        self.echo_line = if config.echo {
            self.delay_line(100.0 + 180.0 * config.strength)
        } else {
            Vec::new()
        };
        self.reverb_line = if config.reverb {
            self.delay_line(43.0 + 24.0 * config.strength)
        } else {
            Vec::new()
        };
        self.echo_cursor = 0;
        self.reverb_cursor = 0;
        self.applied_version = config.version;
    }

    fn delay_line(&self, milliseconds: f32) -> Vec<f32> {
        let frames = ((self.sample_rate as f32 * milliseconds / 1_000.0).round() as usize).max(1);
        vec![0.0; frames * self.channels]
    }

    fn is_passthrough(&self) -> bool {
        self.config.preset == EffectsPreset::None
            && !self.config.echo
            && !self.config.reverb
            && !self.config.limiter
    }

    fn reset_delay_lines(&mut self) {
        self.echo_line.fill(0.0);
        self.reverb_line.fill(0.0);
        self.echo_cursor = 0;
        self.reverb_cursor = 0;
    }
}

fn effect_gains(config: &EffectsConfig) -> [f32; 5] {
    let base = match config.preset {
        EffectsPreset::None => [0.0; 5],
        EffectsPreset::BassBoost => [8.0, 5.0, 2.0, -1.0, -2.0],
        EffectsPreset::VocalBoost => [-3.0, -1.0, 3.0, 5.0, 2.0],
        EffectsPreset::Warm => [4.0, 3.0, 1.0, -1.0, -2.0],
        EffectsPreset::Equalizer => config.equalizer_gains_db,
    };
    base.map(|gain| gain * config.strength)
}

fn effect_headroom(config: &EffectsConfig, gains: &[f32; 5]) -> f32 {
    let maximum_boost = gains.iter().copied().fold(0.0f32, f32::max);
    let equalizer = 10.0f32.powf(-maximum_boost / 20.0);
    let echo = if config.echo { 0.95 } else { 1.0 };
    let reverb = if config.reverb {
        1.0 / (1.0 + 0.08 + 0.14 * config.strength)
    } else {
        1.0
    };
    (equalizer * echo * reverb).clamp(0.1, 1.0)
}

fn soft_limit(sample: f32) -> f32 {
    const KNEE: f32 = 0.92;
    let magnitude = sample.abs();
    if magnitude <= KNEE {
        return sample;
    }
    let compressed = KNEE + (1.0 - (-12.0 * (magnitude - KNEE)).exp()) * (1.0 - KNEE);
    sample.signum() * compressed.min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_compatibility_effects_are_bit_exact() {
        let mut processor = SoftwareEffects::new(44_100, 2);
        let input = vec![-0.75, 0.25, 0.5, -0.125];
        assert_eq!(processor.process(&input, &EffectsConfig::default()), input);
    }

    #[test]
    fn maximum_compatibility_effects_remain_finite_and_bounded() {
        let mut processor = SoftwareEffects::new(44_100, 2);
        let config = EffectsConfig {
            preset: EffectsPreset::Equalizer,
            strength: 1.0,
            echo: true,
            reverb: true,
            limiter: true,
            equalizer_gains_db: [18.0; 5],
            version: 1,
        };
        let mut input = vec![0.0; 44_100 * 2];
        input[0] = 1.0;
        input[1] = -1.0;
        let output = processor.process(&input, &config);
        assert!(output.iter().all(|sample| sample.is_finite()));
        assert!(output.iter().all(|sample| sample.abs() <= 1.0));
        assert!(output.iter().any(|sample| sample.abs() > 0.0));
    }
}
