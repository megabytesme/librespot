use std::collections::{HashMap, HashSet};
use std::future::Future;

use bytes::Bytes;

use super::{Method, NO_METRICS_AND_SALT, SpClient};
use crate::{
    Error,
    protocol::{context::Context, context_page::ContextPage},
};

// hm names a service on the authenticated spclient, not a network host.
// In particular, preserve the opaque query of a generated context cursor.
fn endpoint(uri: &str) -> Result<String, Error> {
    if let Some(path) = uri.strip_prefix("hm://") {
        if path.is_empty() || path.starts_with('/') || path.contains('#') {
            return Err(Error::invalid_argument("invalid context service URI"));
        }
        Ok(format!("/{path}"))
    } else if uri.starts_with("spotify:") {
        Ok(format!("/context-resolve/v1/{uri}"))
    } else {
        Err(Error::invalid_argument("unsupported context URI scheme"))
    }
}

fn decode_context(data: &[u8]) -> Result<Context, Error> {
    let json: serde_json::Value = serde_json::from_slice(data)?;
    let text = std::str::from_utf8(data)?;
    let options = protobuf_json_mapping::ParseOptions {
        ignore_unknown_fields: true,
        ..Default::default()
    };
    if json.get("pages").is_none()
        && [
            "tracks",
            "page_url",
            "next_page_url",
            "pageUrl",
            "nextPageUrl",
        ]
        .iter()
        .any(|key| json.get(key).is_some())
    {
        let page =
            protobuf_json_mapping::parse_from_str_with_options::<ContextPage>(text, &options)?;
        Ok(Context {
            pages: vec![page],
            ..Default::default()
        })
    } else {
        Ok(protobuf_json_mapping::parse_from_str_with_options(
            text, &options,
        )?)
    }
}

fn continuation(context: &Context, visited: &HashSet<String>) -> Option<String> {
    context
        .url
        .iter()
        .chain(context.metadata.get("lexicon_context_url"))
        .chain(
            context
                .pages
                .iter()
                .flat_map(|page| page.page_url.iter().chain(&page.next_page_url)),
        )
        .find(|url| url.starts_with("hm://") && !visited.contains(url.as_str()))
        .cloned()
}

impl SpClient {
    pub(super) async fn resolve_context(&self, uri: &str) -> Result<Context, Error> {
        resolve_context_with(uri, |endpoint| async move {
            self.request_with_options(&Method::GET, &endpoint, None, None, &NO_METRICS_AND_SALT)
                .await
        })
        .await
    }
}

// Keep the complete resolver path testable without a live account. Fixtures
// exercise the same decoding, cursor traversal and metadata merge as requests.
async fn resolve_context_with<F, Fut>(uri: &str, mut fetch: F) -> Result<Context, Error>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<Bytes, Error>>,
{
    let mut next = uri.to_owned();
    let mut visited = HashSet::new();
    let mut metadata = HashMap::new();
    let mut context_uri = None;
    // Only resolve enough to obtain the initial tracks. Subsequent cursors
    // belong to Connect's lookahead, not an eager traversal of a live DJ.
    for _ in 0..4 {
        visited.insert(next.clone());
        let response = fetch(endpoint(&next)?).await?;
        let mut context = decode_context(&response)?;
        if context_uri.is_none() {
            context_uri = context.uri.clone();
        }
        metadata.extend(context.metadata.clone());
        if context.pages.iter().any(|page| !page.tracks.is_empty()) {
            if next.starts_with("hm://lexicon-session-provider/")
                || metadata.contains_key("lexicon_context_url")
            {
                debug!(target: "librespot_dj", "Resolved DJ context: {} tracks", context.pages.iter().map(|page| page.tracks.len()).sum::<usize>());
            }
            context.metadata = metadata;
            context.uri = context_uri.or(context.uri);
            return Ok(context);
        }
        match continuation(&context, &visited) {
            Some(url) => next = url,
            None => return Ok(context),
        }
    }
    Err(Error::failed_precondition(
        "context resolution exceeded its hop limit",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, future::ready};

    #[tokio::test]
    async fn dj_play_follows_empty_placeholders_and_preserves_narration_and_cursors() {
        let mut replies = VecDeque::from([
            br#"{"uri":"spotify:playlist:example","url":"context://spotify:playlist:example","metadata":{"lexicon_context_url":"hm://lexicon-session-provider/session?cursor=a%2Fb","context_description":"DJ"},"pages":[{}]}"#.as_slice(),
            br#"{"pages":[{"next_page_url":"hm://lexicon-session-provider/page?cursor=c%2Fd"}]}"#.as_slice(),
            br#"{"tracks":[{"uri":"spotify:track:example","uid":"occurrence-one","metadata":{"segment":"one","narration.intro.ssml":"example introduction"}}],"next_page_url":"hm://lexicon-session-provider/later?cursor=e%2Ff"}"#.as_slice(),
        ]);
        let mut requested = Vec::new();
        let context = resolve_context_with("spotify:playlist:example", |endpoint| {
            requested.push(endpoint);
            ready(Ok(Bytes::from_static(
                replies.pop_front().expect("no eager next batch"),
            )))
        })
        .await
        .unwrap();
        assert_eq!(
            requested,
            [
                "/context-resolve/v1/spotify:playlist:example",
                "/lexicon-session-provider/session?cursor=a%2Fb",
                "/lexicon-session-provider/page?cursor=c%2Fd",
            ]
        );
        assert_eq!(context.uri(), "spotify:playlist:example");
        assert_eq!(context.metadata["context_description"], "DJ");
        assert!(context.metadata.contains_key("lexicon_context_url"));
        assert_eq!(context.pages.len(), 1);
        assert_eq!(context.pages[0].tracks[0].uid(), "occurrence-one");
        assert_eq!(
            context.pages[0].tracks[0].metadata["narration.intro.ssml"],
            "example introduction"
        );
        assert_eq!(
            context.pages[0].next_page_url(),
            "hm://lexicon-session-provider/later?cursor=e%2Ff"
        );
    }

    #[tokio::test]
    async fn resolver_timeout_or_service_failure_does_not_poison_a_retry() {
        for failure in [
            Error::deadline_exceeded("fixture timeout"),
            Error::unavailable("fixture unavailable"),
        ] {
            let mut replies = VecDeque::from([
                Ok(Bytes::from_static(br#"{"uri":"spotify:playlist:example","metadata":{"lexicon_context_url":"hm://lexicon-session-provider/session"},"pages":[]}"#)),
                Err(failure),
            ]);
            let error = resolve_context_with("spotify:playlist:example", |_| {
                ready(
                    replies
                        .pop_front()
                        .expect("a failed request must stop traversal"),
                )
            })
            .await
            .unwrap_err();
            assert!(matches!(
                error.kind,
                crate::error::ErrorKind::DeadlineExceeded | crate::error::ErrorKind::Unavailable
            ));
            assert!(replies.is_empty());

            let mut calls = 0;
            let retried = resolve_context_with("spotify:playlist:example", |_| {
                calls += 1;
                ready(Ok(Bytes::from_static(br#"{"uri":"spotify:playlist:example","pages":[{"tracks":[{"uri":"spotify:track:example"}]}]}"#)))
            }).await.unwrap();
            assert_eq!(calls, 1);
            assert_eq!(retried.pages[0].tracks.len(), 1);
        }
    }

    #[tokio::test]
    async fn resolver_bounds_empty_cursor_chains_and_does_not_repeat_a_cycle() {
        let mut calls = 0;
        let error = resolve_context_with("spotify:playlist:example", |_| {
            calls += 1;
            ready(Ok(Bytes::from(format!(
                r#"{{"url":"hm://lexicon-session-provider/page-{calls}","pages":[]}}"#
            ))))
        })
        .await
        .unwrap_err();
        assert_eq!(calls, 4);
        assert_eq!(error.kind, crate::error::ErrorKind::FailedPrecondition);

        calls = 0;
        let empty = resolve_context_with("spotify:playlist:example", |_| {
            calls += 1;
            ready(Ok(Bytes::from_static(
                br#"{"url":"hm://lexicon-session-provider/again","pages":[]}"#,
            )))
        })
        .await
        .unwrap();
        assert_eq!(calls, 2);
        assert!(empty.pages.is_empty());
    }

    #[tokio::test]
    async fn ordinary_playlists_still_resolve_in_one_request_and_bad_json_fails() {
        let mut calls = 0;
        let context = resolve_context_with("spotify:playlist:ordinary", |endpoint| {
            calls += 1;
            assert_eq!(endpoint, "/context-resolve/v1/spotify:playlist:ordinary");
            ready(Ok(Bytes::from_static(br#"{"uri":"spotify:playlist:ordinary","pages":[{"tracks":[{"uri":"spotify:track:example"}],"next_page_url":"hm://service/later"}]}"#)))
        }).await.unwrap();
        assert_eq!(calls, 1);
        assert_eq!(context.pages[0].tracks.len(), 1);
        assert!(
            resolve_context_with("spotify:playlist:example", |_| ready(Ok(
                Bytes::from_static(b"not JSON")
            )))
            .await
            .is_err()
        );
    }

    #[test]
    fn service_cursors_preserve_queries_without_changing_hosts() {
        assert_eq!(endpoint("hm://lexicon-session-provider/context-resolve/v2/session?contextUri=spotify:playlist:test&cursor=a%2Fb").unwrap(),
            "/lexicon-session-provider/context-resolve/v2/session?contextUri=spotify:playlist:test&cursor=a%2Fb");
        assert_eq!(
            endpoint("spotify:album:test").unwrap(),
            "/context-resolve/v1/spotify:album:test"
        );
        for uri in [
            "https://example.org",
            "hm://",
            "hm:///example.org",
            "hm://service/#fragment",
        ] {
            assert!(endpoint(uri).is_err());
        }
    }

    #[test]
    fn page_response_is_a_single_context_page() {
        let context = decode_context(br#"{"tracks":[{"uri":"spotify:track:test","metadata":{"narration.intro.ssml":"example"}}],"next_page_url":"hm://service/next"}"#).unwrap();
        assert_eq!(context.pages.len(), 1);
        assert_eq!(
            context.pages[0].tracks[0].metadata["narration.intro.ssml"],
            "example"
        );
        assert_eq!(context.pages[0].next_page_url(), "hm://service/next");
    }

    #[test]
    fn empty_placeholder_can_resolve_but_never_cycles() {
        let context = decode_context(
            br#"{"uri":"spotify:playlist:test","url":"hm://service/session","pages":[{}]}"#,
        )
        .unwrap();
        let mut visited = HashSet::new();
        assert_eq!(
            continuation(&context, &visited).as_deref(),
            Some("hm://service/session")
        );
        visited.insert("hm://service/session".to_owned());
        assert!(continuation(&context, &visited).is_none());
    }
}
