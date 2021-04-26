use form_urlencoded::Serializer;
use isahc::config::Configurable;
use isahc::http::{method::Method, request::Builder, StatusCode, Uri};
use isahc::{AsyncReadResponseExt, HttpClient, Request};
use serde::de::Deserialize;
use serde_json::from_str;
use std::convert::Into;
use std::marker::PhantomData;
use std::str::FromStr;
use std::sync::Mutex;
use thiserror::Error;

pub use super::api_models::*;
use super::cache::CacheError;

const SPOTIFY_HOST: &str = "api.spotify.com";

fn make_query_params<'a>() -> Serializer<'a, String> {
    Serializer::new(String::new())
}

pub(crate) struct SpotifyRequest<'a, Body, Response> {
    client: &'a SpotifyClient,
    request: Builder,
    body: Body,
    _type: PhantomData<Response>,
}

impl<'a, B, R> SpotifyRequest<'a, B, R>
where
    B: Into<isahc::AsyncBody>,
{
    fn method(mut self, method: Method) -> Self {
        self.request = self.request.method(method);
        self
    }

    fn uri(mut self, path: String, query: Option<&str>) -> Self {
        let path_and_query = match query {
            None => path,
            Some(query) => format!("{}?{}", path, query),
        };
        let uri = Uri::builder()
            .scheme("https")
            .authority(SPOTIFY_HOST)
            .path_and_query(&path_and_query[..])
            .build()
            .unwrap();
        self.request = self.request.uri(uri);
        self
    }

    fn authenticated(mut self) -> Result<Self, SpotifyApiError> {
        let token = self.client.token.lock().unwrap();
        let token = token.as_ref().ok_or(SpotifyApiError::NoToken)?;
        self.request = self
            .request
            .header("Authorization", format!("Bearer {}", token));
        Ok(self)
    }

    pub(crate) fn etag(mut self, etag: Option<String>) -> Self {
        if let Some(etag) = etag {
            self.request = self.request.header("If-None-Match", etag);
        }
        self
    }

    pub(crate) async fn send(self) -> Result<SpotifyResponse<R>, SpotifyApiError> {
        let Self {
            client,
            request,
            body,
            ..
        } = self.authenticated()?;
        client.send_req(request.body(body).unwrap()).await
    }

    pub(crate) async fn send_no_response(self) -> Result<(), SpotifyApiError> {
        let Self {
            client,
            request,
            body,
            ..
        } = self.authenticated()?;
        client
            .send_req_no_response(request.body(body).unwrap())
            .await
    }
}

pub(crate) enum SpotifyResponseKind<T> {
    Ok(String, PhantomData<T>),
    NotModified,
}

pub(crate) struct SpotifyResponse<T> {
    pub kind: SpotifyResponseKind<T>,
    pub max_age: u64,
    pub etag: Option<String>,
}

impl<'a, T> SpotifyResponse<T>
where
    T: Deserialize<'a>,
{
    pub(crate) fn deserialize(&'a self) -> Option<T> {
        if let SpotifyResponseKind::Ok(ref content, _) = self.kind {
            from_str(content).ok()
        } else {
            None
        }
    }
}

#[derive(Error, Debug)]
pub enum SpotifyApiError {
    #[error("Invalid token")]
    InvalidToken,
    #[error("No token")]
    NoToken,
    #[error("No content from request")]
    NoContent,
    #[error("Request failed with status {0}")]
    BadStatus(u16),
    #[error(transparent)]
    ClientError(#[from] isahc::Error),
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    #[error(transparent)]
    CacheError(#[from] CacheError),
    #[error(transparent)]
    ParseError(#[from] serde_json::Error),
    #[error(transparent)]
    ConversionError(#[from] std::string::FromUtf8Error),
}

pub(crate) struct SpotifyClient {
    token: Mutex<Option<String>>,
    client: HttpClient,
}

impl SpotifyClient {
    pub(crate) fn new() -> Self {
        let mut builder = HttpClient::builder();
        if cfg!(debug_assertions) {
            builder = builder.ssl_options(isahc::config::SslOption::DANGER_ACCEPT_INVALID_CERTS);
        }
        let client = builder.build().unwrap();
        Self {
            token: Mutex::new(None),
            client,
        }
    }

    pub(crate) fn request<T>(&self) -> SpotifyRequest<'_, (), T> {
        SpotifyRequest {
            client: self,
            request: Builder::new(),
            body: (),
            _type: PhantomData,
        }
    }

    pub(crate) fn has_token(&self) -> bool {
        self.token.lock().unwrap().is_some()
    }

    pub(crate) fn update_token(&self, new_token: String) {
        if let Ok(mut token) = self.token.lock() {
            *token = Some(new_token)
        }
    }

    fn clear_token(&self) {
        if let Ok(mut token) = self.token.lock() {
            *token = None
        }
    }

    fn parse_cache_control(cache_control: &str) -> Option<u64> {
        cache_control
            .split(',')
            .find(|s| s.trim().starts_with("max-age="))
            .and_then(|s| s.split('=').nth(1))
            .and_then(|s| u64::from_str(s).ok())
    }

    async fn send_req<B, T>(
        &self,
        request: Request<B>,
    ) -> Result<SpotifyResponse<T>, SpotifyApiError>
    where
        B: Into<isahc::AsyncBody>,
    {
        let mut result = self.client.send_async(request).await?;

        let etag = result
            .headers()
            .get("etag")
            .and_then(|header| header.to_str().ok())
            .map(|s| s.to_owned());

        let cache_control = result
            .headers()
            .get("cache-control")
            .and_then(|header| header.to_str().ok())
            .and_then(|s| Self::parse_cache_control(s));

        match result.status() {
            s if s.is_success() => Ok(SpotifyResponse {
                kind: SpotifyResponseKind::Ok(result.text().await?, PhantomData),
                max_age: cache_control.unwrap_or(10),
                etag,
            }),
            StatusCode::UNAUTHORIZED => {
                self.clear_token();
                Err(SpotifyApiError::InvalidToken)
            }
            StatusCode::NOT_MODIFIED => Ok(SpotifyResponse {
                kind: SpotifyResponseKind::NotModified,
                max_age: cache_control.unwrap_or(10),
                etag,
            }),
            s => Err(SpotifyApiError::BadStatus(s.as_u16())),
        }
    }

    async fn send_req_no_response<B>(&self, request: Request<B>) -> Result<(), SpotifyApiError>
    where
        B: Into<isahc::AsyncBody>,
    {
        let result = self.client.send_async(request).await?;
        match result.status() {
            StatusCode::UNAUTHORIZED => {
                self.clear_token();
                Err(SpotifyApiError::InvalidToken)
            }
            StatusCode::NOT_MODIFIED => Ok(()),
            s if s.is_success() => Ok(()),
            s => Err(SpotifyApiError::BadStatus(s.as_u16())),
        }
    }
}

impl SpotifyClient {
    pub(crate) fn get_artist(&self, id: &str) -> SpotifyRequest<'_, (), Artist> {
        self.request()
            .method(Method::GET)
            .uri(format!("/v1/artists/{}", id), None)
    }

    pub(crate) fn get_artist_albums(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> SpotifyRequest<'_, (), Page<Album>> {
        let query = make_query_params()
            .append_pair("include_groups", "album,single")
            .append_pair("country", "from_token")
            .append_pair("offset", &offset.to_string()[..])
            .append_pair("limit", &limit.to_string()[..])
            .finish();

        self.request()
            .method(Method::GET)
            .uri(format!("/v1/artists/{}/albums", id), Some(&query))
    }

    pub(crate) fn get_artist_top_tracks(&self, id: &str) -> SpotifyRequest<'_, (), TopTracks> {
        let query = make_query_params()
            .append_pair("market", "from_token")
            .finish();

        self.request()
            .method(Method::GET)
            .uri(format!("/v1/artists/{}/top-tracks", id), Some(&query))
    }

    pub(crate) fn is_album_saved(&self, id: &str) -> SpotifyRequest<'_, (), Vec<bool>> {
        let query = make_query_params().append_pair("ids", id).finish();
        self.request()
            .method(Method::GET)
            .uri("/v1/me/albums/contains".to_string(), Some(&query))
    }

    pub(crate) fn save_album(&self, id: &str) -> SpotifyRequest<'_, (), ()> {
        let query = make_query_params().append_pair("ids", id).finish();
        self.request()
            .method(Method::PUT)
            .uri("/v1/me/albums".to_string(), Some(&query))
    }

    pub(crate) fn remove_saved_album(&self, id: &str) -> SpotifyRequest<'_, (), ()> {
        let query = make_query_params().append_pair("ids", id).finish();
        self.request()
            .method(Method::DELETE)
            .uri("/v1/me/albums".to_string(), Some(&query))
    }

    pub(crate) fn get_album(&self, id: &str) -> SpotifyRequest<'_, (), Album> {
        self.request()
            .method(Method::GET)
            .uri(format!("/v1/albums/{}", id), None)
    }

    pub(crate) fn get_playlist(&self, id: &str) -> SpotifyRequest<'_, (), Playlist> {
        let query = make_query_params()
            .append_pair(
                "fields",
                "id,name,images,owner,tracks(total,items(is_local,track(name,id,duration_ms,artists(name,id),album(name,id,images,artists))))",
            )
            .finish();
        self.request()
            .method(Method::GET)
            .uri(format!("/v1/playlists/{}", id), Some(&query))
    }

    pub(crate) fn get_playlist_tracks(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> SpotifyRequest<'_, (), Page<PlaylistTrack>> {
        let query = make_query_params()
            .append_pair("offset", &offset.to_string()[..])
            .append_pair("limit", &limit.to_string()[..])
            .finish();

        self.request()
            .method(Method::GET)
            .uri(format!("/v1/playlists/{}/tracks", id), Some(&query))
    }

    pub(crate) fn get_saved_albums(
        &self,
        offset: usize,
        limit: usize,
    ) -> SpotifyRequest<'_, (), Page<SavedAlbum>> {
        let query = make_query_params()
            .append_pair("offset", &offset.to_string()[..])
            .append_pair("limit", &limit.to_string()[..])
            .finish();

        self.request()
            .method(Method::GET)
            .uri("/v1/me/albums".to_string(), Some(&query))
    }

    pub(crate) fn get_saved_playlists(
        &self,
        offset: usize,
        limit: usize,
    ) -> SpotifyRequest<'_, (), Page<Playlist>> {
        let query = make_query_params()
            .append_pair("offset", &offset.to_string()[..])
            .append_pair("limit", &limit.to_string()[..])
            .finish();

        self.request()
            .method(Method::GET)
            .uri("/v1/me/playlists".to_string(), Some(&query))
    }

    pub(crate) fn search(
        &self,
        query: String,
        offset: usize,
        limit: usize,
    ) -> SpotifyRequest<'_, (), RawSearchResults> {
        let query = SearchQuery {
            query,
            types: vec![SearchType::Album, SearchType::Artist],
            limit,
            offset,
        };

        self.request()
            .method(Method::GET)
            .uri("/v1/search".to_string(), Some(&query.into_query_string()))
    }

    pub(crate) fn get_user(&self, id: &str) -> SpotifyRequest<'_, (), User> {
        self.request()
            .method(Method::GET)
            .uri(format!("/v1/users/{}", id), None)
    }

    pub(crate) fn get_user_playlists(
        &self,
        id: &str,
        offset: usize,
        limit: usize,
    ) -> SpotifyRequest<'_, (), Page<Playlist>> {
        let query = make_query_params()
            .append_pair("offset", &offset.to_string()[..])
            .append_pair("limit", &limit.to_string()[..])
            .finish();

        self.request()
            .method(Method::GET)
            .uri(format!("/v1/users/{}/playlists", id), Some(&query))
    }
}

#[cfg(test)]
pub mod tests {

    use super::*;

    #[test]
    fn test_search_query() {
        let query = SearchQuery {
            query: "test".to_string(),
            types: vec![SearchType::Album, SearchType::Artist],
            limit: 5,
            offset: 0,
        };

        assert_eq!(
            query.into_query_string(),
            "type=album,artist&q=test&offset=0&limit=5&market=from_token"
        );
    }

    #[test]
    fn test_search_query_spaces_and_stuff() {
        let query = SearchQuery {
            query: "test??? wow".to_string(),
            types: vec![SearchType::Album],
            limit: 5,
            offset: 0,
        };

        assert_eq!(
            query.into_query_string(),
            "type=album&q=test+wow&offset=0&limit=5&market=from_token"
        );
    }

    #[test]
    fn test_search_query_encoding() {
        let query = SearchQuery {
            query: "кириллица".to_string(),
            types: vec![SearchType::Album],
            limit: 5,
            offset: 0,
        };

        assert_eq!(query.into_query_string(), "type=album&q=%D0%BA%D0%B8%D1%80%D0%B8%D0%BB%D0%BB%D0%B8%D1%86%D0%B0&offset=0&limit=5&market=from_token");
    }
}
