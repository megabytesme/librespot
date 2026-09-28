//! Optional Spotify narration on the existing decoder/output path.
//!
//! This implementation uses librespot's packet positions and normalization;
//! speech never adds to the reported duration of a song.

use std::{collections::HashMap, io::Cursor, time::Duration};

use crate::{
    SAMPLE_RATE, SAMPLES_PER_SECOND,
    config::PlayerConfig,
    core::Session,
    decoder::{AudioDecoder, AudioPacket, AudioPacketPosition, DecoderResult, SymphoniaDecoder},
    player::NormalisationData,
};
use librespot_protocol::{
    client_tts::TtsRequest,
    tts_resolve::resolve_request::{AudioFormat, TtsProvider, TtsVoice},
};
use protobuf::Enum;
use symphonia::core::probe::Hint;

type Decoder = Box<dyn AudioDecoder + Send>;

#[derive(Clone, PartialEq)]
struct Script {
    request: TtsRequest,
    loudness: f64,
    peak: f64,
}

/// Parsed per-occurrence metadata, also used to distinguish a preloaded track
/// from another occurrence of the same song with a different introduction.
#[derive(Clone, Default, PartialEq)]
pub struct Narration {
    before: Option<Script>,
    after: Option<Script>,
}

impl Narration {
    /// `jump` selects a deliberate jump to a chosen track, not an automatic
    /// transition. A load beginning mid-song does not synthesize any speech.
    pub fn from_metadata(metadata: &HashMap<String, String>, jump: bool, position_ms: u32) -> Self {
        if position_ms != 0 {
            return Self::default();
        }
        Self {
            before: Script::parse(
                metadata,
                if jump {
                    "narration.jump"
                } else {
                    "narration.intro"
                },
            ),
            after: Script::parse(metadata, "narration.outro"),
        }
    }

    pub(crate) async fn attach(
        self,
        session: &Session,
        config: &PlayerConfig,
        decoder: Decoder,
        duration_ms: u32,
    ) -> Decoder {
        debug!(target: "librespot_dj", "Narration plan: before={}, after={}", self.before.is_some(), self.after.is_some());
        if config.passthrough || (self.before.is_none() && self.after.is_none()) {
            return decoder;
        }
        let (before, after) = futures_util::future::join(
            Speech::load(self.before, session, config),
            Speech::load(self.after, session, config),
        )
        .await;
        if before.is_none() && after.is_none() {
            decoder
        } else {
            Box::new(NarratedDecoder::new(decoder, before, after, duration_ms))
        }
    }
}

impl Script {
    fn parse(metadata: &HashMap<String, String>, prefix: &str) -> Option<Self> {
        let get = |key| metadata.get(&format!("{prefix}.{key}")).map(String::as_str);
        let ssml = get("ssml").filter(|text| !text.trim().is_empty() && text.len() <= 64 * 1024)?;
        let mut request = TtsRequest {
            audio_format: AudioFormat::MP3.into(),
            tts_voice: get("voice")
                .map_or(Some(TtsVoice::VOICE1), TtsVoice::from_str)?
                .into(),
            tts_provider: get("tts_provider")
                .map_or(Some(TtsProvider::SONANTIC_FAST), TtsProvider::from_str)?
                .into(),
            sample_rate_hz: SAMPLE_RATE as i32,
            ..Default::default()
        };
        request.set_ssml(ssml.to_owned());
        let level = |name, fallback| {
            get(name)
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite() && (-120.0..=6.0).contains(value))
                .unwrap_or(fallback)
        };
        Some(Self {
            request,
            loudness: level("loudness", -16.0),
            peak: level("true_peak", -3.0),
        })
    }
}

struct Speech {
    decoder: Decoder,
    gain: f64,
    remaining_samples: usize,
}

impl Speech {
    async fn load(
        script: Option<Script>,
        session: &Session,
        config: &PlayerConfig,
    ) -> Option<Self> {
        let script = script?;
        let operation = async {
            let bytes = session
                .spclient()
                .get_narration_audio(&script.request)
                .await?;
            let mut hint = Hint::new();
            hint.with_extension("mp3");
            let decoder = SymphoniaDecoder::new_narration(Cursor::new(bytes), hint)?;
            let gain_db = -14.0 - script.loudness;
            let peak = 10.0_f64.powf(script.peak / 20.0);
            let gain = NormalisationData::get_factor(
                config,
                NormalisationData {
                    track_gain_db: gain_db,
                    album_gain_db: gain_db,
                    track_peak: peak,
                    album_peak: peak,
                },
            );
            Ok::<_, crate::core::Error>(Self {
                decoder: Box::new(decoder),
                gain,
                remaining_samples: SAMPLES_PER_SECOND as usize * 120,
            })
        };
        match tokio::time::timeout(Duration::from_secs(8), operation).await {
            Ok(Ok(speech)) => Some(speech),
            _ => {
                // No script, signed URL, response body, or account metadata in logs.
                warn!(target: "librespot_dj", "Narration unavailable; continuing with music");
                None
            }
        }
    }

    fn packet(&mut self) -> Option<AudioPacket> {
        if self.remaining_samples == 0 {
            return None;
        }
        match self.decoder.next_packet() {
            Ok(Some((_, AudioPacket::Samples(samples))))
                if samples.len() <= self.remaining_samples =>
            {
                self.remaining_samples -= samples.len();
                Some(AudioPacket::Samples(samples))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Before,
    Song,
    After,
    End,
}

struct NarratedDecoder {
    song: Decoder,
    before: Option<Speech>,
    after: Option<Speech>,
    phase: Phase,
    duration_ms: u32,
    packet_gain: Option<f64>,
}

impl NarratedDecoder {
    fn new(song: Decoder, before: Option<Speech>, after: Option<Speech>, duration_ms: u32) -> Self {
        Self {
            song,
            before,
            after,
            phase: Phase::Before,
            duration_ms,
            packet_gain: None,
        }
    }
}

impl AudioDecoder for NarratedDecoder {
    fn seek(&mut self, position_ms: u32) -> Result<u32, crate::decoder::DecoderError> {
        let position = self.song.seek(position_ms)?;
        self.phase = Phase::Song;
        self.before = None;
        self.packet_gain = None;
        Ok(position)
    }

    fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
        self.packet_gain = None;
        loop {
            let (speech, position_ms, next) = match self.phase {
                Phase::Before => (&mut self.before, 0, Phase::Song),
                Phase::After => (&mut self.after, self.duration_ms, Phase::End),
                Phase::End => return Ok(None),
                Phase::Song => match self.song.next_packet()? {
                    Some(packet) => return Ok(Some(packet)),
                    None => {
                        self.phase = Phase::After;
                        continue;
                    }
                },
            };
            if let Some(speech) = speech {
                if let Some(packet) = speech.packet() {
                    self.packet_gain = Some(speech.gain);
                    return Ok(Some((
                        AudioPacketPosition {
                            position_ms,
                            skipped: false,
                        },
                        packet,
                    )));
                }
            }
            self.phase = next;
        }
    }

    fn is_narration(&self) -> bool {
        self.packet_gain.is_some()
    }
    fn normalisation_override(&self) -> Option<f64> {
        self.packet_gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder::DecoderError;
    use std::collections::VecDeque;

    struct Packets(VecDeque<DecoderResult<Option<(AudioPacketPosition, AudioPacket)>>>);
    impl AudioDecoder for Packets {
        fn seek(&mut self, position: u32) -> Result<u32, DecoderError> {
            Ok(position)
        }
        fn next_packet(&mut self) -> DecoderResult<Option<(AudioPacketPosition, AudioPacket)>> {
            self.0.pop_front().unwrap_or(Ok(None))
        }
    }
    fn decoder(value: f64, position_ms: u32) -> Decoder {
        Box::new(Packets(VecDeque::from([Ok(Some((
            AudioPacketPosition {
                position_ms,
                skipped: false,
            },
            AudioPacket::Samples(vec![value; 4]),
        )))])))
    }
    fn speech(value: f64) -> Option<Speech> {
        Some(Speech {
            decoder: decoder(value, 999),
            gain: 0.5,
            remaining_samples: 32,
        })
    }
    fn next(decoder: &mut NarratedDecoder) -> (u32, f64, bool) {
        let (position, packet) = decoder.next_packet().unwrap().unwrap();
        (
            position.position_ms,
            packet.samples().unwrap()[0],
            decoder.is_narration(),
        )
    }

    #[test]
    fn packet_positions_and_gain_belong_to_the_song_or_speech() {
        let mut decoder = NarratedDecoder::new(decoder(0.2, 32), speech(0.1), speech(0.3), 100);
        assert_eq!(next(&mut decoder), (0, 0.1, true));
        assert_eq!(decoder.normalisation_override(), Some(0.5));
        assert_eq!(next(&mut decoder), (32, 0.2, false));
        assert_eq!(decoder.normalisation_override(), None);
        assert_eq!(next(&mut decoder), (100, 0.3, true));
        assert!(decoder.next_packet().unwrap().is_none());
    }

    #[test]
    fn seeking_at_zero_also_discards_the_introduction() {
        let mut decoder = NarratedDecoder::new(decoder(0.2, 0), speech(0.1), speech(0.3), 100);
        assert_eq!(decoder.seek(0).unwrap(), 0);
        assert_eq!(next(&mut decoder), (0, 0.2, false));
        assert_eq!(next(&mut decoder), (100, 0.3, true));
    }

    #[test]
    fn broken_speech_is_optional_but_song_errors_propagate() {
        let broken = || {
            Box::new(Packets(VecDeque::from([Err(
                DecoderError::SymphoniaDecoder("test failure".into()),
            )]))) as Decoder
        };
        let before = Speech {
            decoder: broken(),
            gain: 1.0,
            remaining_samples: 32,
        };
        let mut narrated = NarratedDecoder::new(decoder(0.2, 0), Some(before), None, 100);
        assert_eq!(next(&mut narrated), (0, 0.2, false));
        let mut narrated = NarratedDecoder::new(broken(), None, speech(0.3), 100);
        assert!(narrated.next_packet().is_err());
    }

    #[test]
    fn plans_are_per_occurrence_and_mid_song_loads_are_plain() {
        let metadata = HashMap::from([
            ("narration.intro.ssml".into(), "intro example".into()),
            ("narration.jump.ssml".into(), "jump example".into()),
            ("narration.intro.loudness".into(), "NaN".into()),
        ]);
        let normal = Narration::from_metadata(&metadata, false, 0);
        let jumped = Narration::from_metadata(&metadata, true, 0);
        assert!(normal != jumped);
        assert_eq!(normal.before.as_ref().unwrap().loudness, -16.0);
        assert_eq!(
            jumped.before.as_ref().unwrap().request.ssml(),
            "jump example"
        );
        assert!(Narration::from_metadata(&metadata, false, 1) == Narration::default());
        let metadata = HashMap::from([
            ("narration.intro.ssml".into(), "example".into()),
            ("narration.intro.voice".into(), "UNSUPPORTED_VOICE".into()),
        ]);
        assert!(Narration::from_metadata(&metadata, false, 0) == Narration::default());
    }
}
