use bytes::Bytes;
use http_body_util::{BodyExt, Limited};
use hyper::{
    Request, StatusCode,
    header::{AUTHORIZATION, CONTENT_TYPE, LOCATION},
};
use protobuf::Message;

use super::{CLIENT_TOKEN, SpClient};
use crate::{Error, protocol::client_tts::TtsRequest};

const MAX_CLIP_BYTES: usize = 8 * 1024 * 1024;

async fn clip_bytes<B>(body: B) -> Result<Bytes, Error>
where
    B: hyper::body::Body<Data = Bytes>,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let data = Limited::new(body, MAX_CLIP_BYTES)
        .collect()
        .await
        .map_err(|_| {
            Error::unavailable("narration audio was incomplete or exceeded the size limit")
        })?
        .to_bytes();
    if data.is_empty() {
        return Err(Error::unavailable("narration audio was empty"));
    }
    Ok(data)
}

fn audio_request(location: &str) -> Result<Request<Bytes>, Error> {
    let url = url::Url::parse(location)
        .map_err(|_| Error::failed_precondition("invalid narration URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::failed_precondition(
            "narration requires an HTTPS URL without credentials",
        ));
    }
    // Start with a new request: signed media URLs must not inherit any Spotify
    // authentication headers. Hyper does not follow further redirects.
    Ok(Request::get(location).body(Bytes::new())?)
}

impl SpClient {
    /// Fetch one Spotify-supplied narration script as MP3. Callers must bound
    /// the entire operation with a timeout and treat failure as optional audio.
    pub async fn get_narration_audio(&self, script: &TtsRequest) -> Result<Bytes, Error> {
        let token = self.session().login5().auth_token().await?;
        let mut request =
            Request::post(format!("{}/client-tts/v1/fulfill", self.base_url().await?))
                .header(CONTENT_TYPE, "application/x-protobuf")
                .header(
                    AUTHORIZATION,
                    format!("{} {}", token.token_type, token.access_token),
                )
                .body(Bytes::from(script.write_to_bytes()?))?;
        if let Ok(token) = self.client_token().await {
            if !token.is_empty() {
                request.headers_mut().insert(CLIENT_TOKEN, token.parse()?);
            }
        }
        let response = self
            .session()
            .http_client()
            .request_redirect(request)
            .await?;
        if !matches!(response.status(), StatusCode::FOUND | StatusCode::SEE_OTHER) {
            return Err(Error::failed_precondition(
                "narration service did not return an audio location",
            ));
        }
        let location = response
            .headers()
            .get(LOCATION)
            .and_then(|value| value.to_str().ok())
            .ok_or_else(|| {
                Error::failed_precondition("narration response has no audio location")
            })?;
        let request = audio_request(location)?;
        // request_fut applies the shared rate limiter without logging the signed URL.
        let response = self.session().http_client().request_fut(request)?.await?;
        if response.status() != StatusCode::OK {
            return Err(Error::unavailable("narration audio download failed"));
        }
        clip_bytes(response.into_body()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clips_are_bounded_and_empty_downloads_are_not_audio() {
        use http_body_util::Full;
        assert!(clip_bytes(Full::new(Bytes::new())).await.is_err());
        let bytes = Bytes::from(vec![0; MAX_CLIP_BYTES + 1]);
        assert!(clip_bytes(Full::new(bytes.clone())).await.is_err());
        assert_eq!(
            clip_bytes(Full::new(bytes.slice(..MAX_CLIP_BYTES)))
                .await
                .unwrap()
                .len(),
            MAX_CLIP_BYTES
        );
    }

    #[test]
    fn media_requests_never_carry_service_credentials() {
        let request = audio_request("https://cdn.example.test/clip.mp3?signature=test").unwrap();
        assert_eq!(request.method(), hyper::Method::GET);
        assert!(request.headers().is_empty());
        for url in [
            "http://cdn.example.test/audio",
            "https://user:password@cdn.example.test/audio",
            "file:///audio",
            "https://cdn.example.test/audio#fragment",
        ] {
            assert!(audio_request(url).is_err());
        }
    }
}
