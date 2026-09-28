//! DJ set changes use the boundaries supplied by Spotify, not ordinary Next.

use super::{ConnectState, context::ContextType, provider::IsProvider};
use crate::{core::Error, playback::player::DjSet};

impl ConnectState {
    pub fn is_dj(&self) -> bool {
        self.player()
            .context_metadata
            .contains_key("lexicon_context_url")
            || self
                .player()
                .context_url
                .starts_with("hm://lexicon-session-provider/")
    }

    pub fn next_dj_set(&self) -> Option<DjSet> {
        if !self.is_dj()
            || self.player().options.shuffling_context
            || self.player().options.repeating_context
            || self.player().options.repeating_track
        {
            return None;
        }
        let current_segment = self.player().track.metadata.get("segment");
        let mut consumed = Vec::new();
        for track in &self.player().next_tracks {
            if track.is_queue() || track.is_unavailable() || !track.is_context() {
                continue;
            }
            consumed.push(track.uri.clone());
            let different_segment = track
                .metadata
                .get("segment")
                .is_some_and(|segment| Some(segment) != current_segment);
            let has_jump = track
                .metadata
                .get("narration.jump.ssml")
                .is_some_and(|script| !script.trim().is_empty());
            if different_segment && has_jump && !track.uid.is_empty() {
                return Some(DjSet {
                    uri: track.uri.clone(),
                    uid: track.uid.clone(),
                    consumed,
                });
            }
        }
        None
    }

    pub fn jump_to_dj_set(&mut self, expected_uid: &str) -> Result<(), Error> {
        let next = self
            .next_dj_set()
            .filter(|next| next.uid == expected_uid)
            .ok_or_else(|| Error::failed_precondition("DJ set changed or is not ready"))?;
        let context = self.get_context(ContextType::Default)?;
        let index = Self::find_index_in_context(context, |track| track.uid == next.uid)?;
        self.set_repeat_track(false);
        self.set_repeat_context(false);
        self.set_active_context(ContextType::Default);
        self.set_current_track(index)?;
        // This retains the user's queue and discards only the old context tail.
        self.reset_playback_to_position(Some(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{Session, SessionConfig},
        protocol::{
            context::Context, context_page::ContextPage, context_track::ContextTrack,
            player::ProvidedTrack,
        },
        state::ConnectConfig,
    };

    fn track(uid: &str, segment: &str, jump: bool) -> ContextTrack {
        let mut metadata = std::collections::HashMap::from([("segment".into(), segment.into())]);
        if jump {
            metadata.insert("narration.jump.ssml".into(), "example jump".into());
        }
        ContextTrack {
            uri: Some("spotify:track:0000000000000000000001".into()),
            uid: Some(uid.into()),
            metadata,
            ..Default::default()
        }
    }

    fn dj_context() -> Context {
        Context {
            uri: Some("spotify:playlist:example".into()),
            metadata: std::collections::HashMap::from([(
                "lexicon_context_url".into(),
                "hm://lexicon-session-provider/example".into(),
            )]),
            pages: vec![ContextPage {
                tracks: vec![
                    track("a", "one", false),
                    track("b", "one", false),
                    track("c", "two", true),
                    track("d", "two", false),
                ],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn dj_ignores_shuffle_but_ordinary_playback_still_shuffles() {
        let session = Session::new(SessionConfig::default(), None);
        let mut state = ConnectState::new(ConnectConfig::default(), &session);
        state.set_shuffle(true);
        state
            .update_context(dj_context(), ContextType::Default)
            .unwrap();
        state.set_active_context(ContextType::Default);
        assert!(
            !state.shuffling_context(),
            "entering DJ clears inherited shuffle"
        );
        state.set_shuffle(true); // Explicit shuffle in a Load request.
        assert!(!state.shuffling_context());
        state.set_current_track(1).unwrap();
        state.reset_playback_to_position(Some(1)).unwrap();
        state.add_to_queue(
            ProvidedTrack {
                uri: "spotify:track:0000000000000000000002".into(),
                ..Default::default()
            },
            true,
        );
        let before = state.player().next_tracks.clone();
        for requested in [true, false, true] {
            state.handle_shuffle(requested).unwrap();
            assert!(!state.shuffling_context());
            assert_eq!(state.player().track.uid, "b");
            assert_eq!(state.player().next_tracks, before);
            assert_eq!(state.next_dj_set().unwrap().uid, "c");
        }
        state.set_repeat_context(true);
        state.handle_shuffle(true).unwrap();
        assert!(state.repeat_context(), "only shuffle is automatic");

        state.reset_context(super::super::context::ResetContext::Completely);
        let mut ordinary = dj_context();
        ordinary.metadata.clear();
        state
            .update_context(ordinary, ContextType::Default)
            .unwrap();
        state.set_active_context(ContextType::Default);
        state.set_current_track(0).unwrap();
        state.handle_shuffle(true).unwrap();
        assert!(state.shuffling_context());
    }

    #[tokio::test]
    async fn shuffled_dj_transfers_restore_order_and_keep_the_current_occurrence() {
        use crate::protocol::{
            context_player_options::ContextPlayerOptions, playback::Playback, queue::Queue,
            session::Session as TransferSession, transfer_state::TransferState,
        };
        use protobuf::MessageField;

        // DJ can be identified by metadata, its resolver URL, or only once
        // asynchronous context resolution finishes.
        for marker in ["metadata", "url", "resolved"] {
            let session = Session::new(SessionConfig::default(), None);
            let mut state = ConnectState::new(ConnectConfig::default(), &session);
            let resolved = dj_context();
            let mut incoming = resolved.clone();
            if marker != "metadata" {
                incoming.metadata.clear();
            }
            if marker == "url" {
                incoming.url = Some("hm://lexicon-session-provider/example".into());
            }
            let mut queued = track("queued", "", false);
            queued.uri = Some("spotify:track:0000000000000000000002".into());
            let mut transfer = TransferState {
                options: MessageField::some(ContextPlayerOptions {
                    shuffling_context: Some(true),
                    ..Default::default()
                }),
                playback: MessageField::some(Playback {
                    current_track: MessageField::some(track("b", "one", false)),
                    is_paused: Some(true),
                    position_as_of_timestamp: Some(42_000),
                    ..Default::default()
                }),
                current_session: MessageField::some(TransferSession {
                    context: MessageField::some(incoming),
                    ..Default::default()
                }),
                queue: MessageField::some(Queue {
                    tracks: vec![queued],
                    ..Default::default()
                }),
                ..Default::default()
            };
            state.set_track(state.current_track_from_transfer(&transfer).unwrap());
            state.handle_initial_transfer(&mut transfer, resolved.uri.clone());
            if marker != "resolved" {
                assert!(!state.shuffling_context(), "{marker}");
            }
            state
                .update_context(resolved, ContextType::Default)
                .unwrap();
            state.finish_transfer(transfer).unwrap();
            assert!(!state.shuffling_context(), "{marker}");
            assert!(state.player().is_paused);
            assert_eq!(state.player().track.uid, "b");
            assert_eq!(state.player().index.track, 1);
            let next = &state.player().next_tracks;
            assert!(next[0].is_queue());
            assert_eq!(next[0].uri, "spotify:track:0000000000000000000002");
            assert_eq!(next.len(), 3);
            assert_eq!(next[1].uid, "c");
            assert_eq!(next[2].uid, "d");
            assert_eq!(state.next_dj_set().unwrap().uid, "c");
        }
    }

    #[tokio::test]
    async fn set_jump_is_occurrence_aware_and_keeps_the_manual_queue() {
        let session = Session::new(SessionConfig::default(), None);
        let mut state = ConnectState::new(ConnectConfig::default(), &session);
        state
            .update_context(dj_context(), ContextType::Default)
            .unwrap();
        state.set_active_context(ContextType::Default);
        state.set_current_track(0).unwrap();
        state.reset_playback_to_position(Some(0)).unwrap();
        state.add_to_queue(
            ProvidedTrack {
                uri: "spotify:track:0000000000000000000002".into(),
                ..Default::default()
            },
            true,
        );
        let next = state.next_dj_set().unwrap();
        state.set_repeat_context(true);
        assert!(state.next_dj_set().is_none());
        state.set_repeat_context(false);
        assert_eq!(next.uid, "c");
        assert_eq!(next.consumed.len(), 2);
        assert!(state.jump_to_dj_set("stale").is_err());
        assert_eq!(state.player().track.uid, "a");
        state.jump_to_dj_set("c").unwrap();
        assert_eq!(state.player().track.uid, "c");
        assert!(state.player().next_tracks[0].is_queue());
        assert_eq!(state.player().next_tracks[1].uid, "d");
        assert!(state.next_dj_set().is_none());
        state.player_mut().context_metadata.clear();
        assert!(!state.is_dj());
    }
}
