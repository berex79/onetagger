use anyhow::{Context, Error};
use chrono::{DateTime, NaiveDate, Utc};
use curl::easy::Easy;
use onetagger_tag::FrameName;
use onetagger_tagger::{
    supported_tags, Album, AudioFileInfo, AutotaggerSource, AutotaggerSourceBuilder, MatchingUtils,
    PlatformCustomOptionValue, PlatformCustomOptions, PlatformInfo, SupportedTag, TaggerConfig,
    Track, TrackMatch, TrackNumber,
};
use regex::Regex;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::{
    header::{CONTENT_TYPE, RETRY_AFTER},
    StatusCode,
};
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::sleep;
use std::time::Duration;

const INVALID_ART: &'static str = "ab2d1d04-233d-4b08-8234-9782b34dcab8";
const SEARCH_URL: &str = "https://www.beatport.com/search/tracks";
const API_URL: &str = "https://api.beatport.com/v4/catalog";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/138.0.0.0 Safari/537.36";
const MAX_RETRIES: usize = 2;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

pub struct Beatport {
    client: Client,
    access_token: Arc<Mutex<Option<BeatportToken>>>,
}

impl Beatport {
    /// Create new instance
    fn new(access_token: Arc<Mutex<Option<BeatportToken>>>) -> Beatport {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();
        Beatport {
            client,
            access_token,
        }
    }

    /// Search for tracks on beatport
    pub fn search(
        &self,
        query: &str,
        page: i32,
        results_per_page: usize,
    ) -> Result<BeatportTrackResults, Error> {
        let query = Self::clear_search_query(query);
        let parsed = self.fetch_search_page(&query, page, results_per_page)?;
        self.cache_session(parsed.session);
        Ok(parsed.results)
    }

    fn fetch_search_page(
        &self,
        query: &str,
        page: i32,
        results_per_page: usize,
    ) -> Result<ParsedSearchPage, Error> {
        let mut url = url::Url::parse(SEARCH_URL).unwrap();
        url.query_pairs_mut()
            .append_pair("q", query)
            .append_pair("page", &page.to_string())
            .append_pair("per-page", &results_per_page.to_string());
        let response = self.send_curl_with_retry("Beatport search", url.as_str())?;
        ensure_success(&response, "search")?;
        parse_search_page(&response.body, response.content_type.as_deref())
    }

    fn cache_session(&self, session: Option<BeatportAnonSession>) {
        let Some(session) = session else { return };
        if session.access_token.trim().is_empty() {
            return;
        }
        let expires_at = session
            .expiration_time
            .filter(|expiry| *expiry > timestamp!())
            .unwrap_or_else(|| timestamp!() + session.expires_in.saturating_mul(1000))
            .saturating_sub(10_000);
        *self.access_token.lock().unwrap() = Some(BeatportToken {
            access_token: session.access_token,
            expires_at,
        });
    }

    /// Return a configured token or obtain the anonymous session issued by the public website.
    fn update_token(&self) -> Result<String, Error> {
        if let Ok(token) = std::env::var("ONETAGGER_BEATPORT_ACCESS_TOKEN") {
            if !token.trim().is_empty() {
                return Ok(token);
            }
        }

        if let Some(token) = self
            .access_token
            .lock()
            .unwrap()
            .as_ref()
            .filter(|token| token.expires_at > timestamp!())
            .map(|token| token.access_token.clone())
        {
            return Ok(token);
        }

        let parsed = self
            .fetch_search_page("a", 1, 1)
            .context("Failed to obtain Beatport anonymous session")?;
        let session = parsed
            .session
            .ok_or_else(|| anyhow!("Beatport search page did not provide an anonymous session"))?;
        let token = session.access_token.clone();
        self.cache_session(Some(session));
        Ok(token)
    }

    fn invalidate_token(&self) {
        *self.access_token.lock().unwrap() = None;
    }

    fn send_api_get(&self, url: &str, operation: &str) -> Result<HttpResponse, Error> {
        let configured_token = std::env::var("ONETAGGER_BEATPORT_ACCESS_TOKEN")
            .is_ok_and(|token| !token.trim().is_empty());
        for auth_attempt in 0..=1 {
            let token = self.update_token()?;
            let response = self.send_with_retry(operation, url, || {
                self.client.get(url).bearer_auth(token.clone())
            })?;
            if response.status == StatusCode::UNAUTHORIZED && auth_attempt == 0 && !configured_token
            {
                self.invalidate_token();
                continue;
            }
            return Ok(response);
        }
        unreachable!()
    }

    fn send_with_retry<F>(
        &self,
        operation: &str,
        requested_url: &str,
        request: F,
    ) -> Result<HttpResponse, Error>
    where
        F: Fn() -> RequestBuilder,
    {
        for attempt in 0..=MAX_RETRIES {
            let response = match request().send() {
                Ok(response) => response,
                Err(error) => {
                    warn!(
                        "{operation}: url={}, transport_error={error}",
                        sanitize_url(requested_url)
                    );
                    return Err(error).with_context(|| format!("{operation} request failed"));
                }
            };
            let status = response.status();
            let headers = response.headers().clone();
            let final_url = sanitize_url(response.url().as_str());
            let content_type = headers
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let retry_after = headers
                .get(RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let body_bytes = response
                .bytes()
                .with_context(|| format!("Failed reading {operation} response body"))?;
            let body = String::from_utf8_lossy(&body_bytes).into_owned();
            log_response(
                operation,
                requested_url,
                &final_url,
                status,
                content_type.as_deref(),
                &body,
            );

            if is_retryable(status) && attempt < MAX_RETRIES {
                let delay = retry_delay(retry_after.as_deref(), attempt);
                warn!(
                    "{operation} returned HTTP {status}; retrying in {}s (attempt {}/{})",
                    delay.as_secs(),
                    attempt + 1,
                    MAX_RETRIES
                );
                sleep(delay);
                continue;
            }
            return Ok(HttpResponse {
                status,
                content_type,
                body,
                retry_after,
            });
        }
        unreachable!()
    }

    fn send_curl_with_retry(
        &self,
        operation: &str,
        requested_url: &str,
    ) -> Result<HttpResponse, Error> {
        for attempt in 0..=MAX_RETRIES {
            let response = Self::curl_get(operation, requested_url)?;
            if is_retryable(response.status) && attempt < MAX_RETRIES {
                let delay = retry_delay(response.retry_after.as_deref(), attempt);
                warn!(
                    "{operation} returned HTTP {}; retrying in {}s (attempt {}/{})",
                    response.status,
                    delay.as_secs(),
                    attempt + 1,
                    MAX_RETRIES
                );
                sleep(delay);
                continue;
            }
            return Ok(response);
        }
        unreachable!()
    }

    fn curl_get(operation: &str, requested_url: &str) -> Result<HttpResponse, Error> {
        let mut easy = Easy::new();
        easy.url(requested_url)
            .with_context(|| format!("Invalid {operation} URL"))?;
        easy.get(true)
            .context("Failed configuring Beatport GET request")?;
        easy.useragent(USER_AGENT)
            .context("Failed configuring Beatport User-Agent")?;
        easy.follow_location(true)
            .context("Failed configuring Beatport redirects")?;
        easy.max_redirections(5)
            .context("Failed configuring Beatport redirect limit")?;
        easy.connect_timeout(Duration::from_secs(10))
            .context("Failed configuring Beatport connect timeout")?;
        easy.timeout(Duration::from_secs(30))
            .context("Failed configuring Beatport request timeout")?;

        let mut body = Vec::new();
        let mut retry_after = None;
        {
            let mut transfer = easy.transfer();
            transfer
                .write_function(|data| {
                    body.extend_from_slice(data);
                    Ok(data.len())
                })
                .context("Failed configuring Beatport response reader")?;
            transfer
                .header_function(|header| {
                    if header.starts_with(b"HTTP/") {
                        retry_after = None;
                    } else if let Ok(header) = std::str::from_utf8(header) {
                        if let Some((name, value)) = header.split_once(':') {
                            if name.eq_ignore_ascii_case("retry-after") {
                                retry_after = Some(value.trim().to_string());
                            }
                        }
                    }
                    true
                })
                .context("Failed configuring Beatport response headers")?;
            if let Err(error) = transfer.perform() {
                warn!(
                    "{operation}: url={}, transport_error={error}",
                    sanitize_url(requested_url)
                );
                return Err(error).with_context(|| format!("{operation} request failed"));
            }
        }

        let status = StatusCode::from_u16(easy.response_code()? as u16)
            .context("Beatport returned an invalid HTTP status")?;
        let content_type = easy.content_type()?.map(str::to_owned);
        let final_url = easy
            .effective_url()?
            .map(sanitize_url)
            .unwrap_or_else(|| sanitize_url(requested_url));
        let body = String::from_utf8_lossy(&body).into_owned();
        log_response(
            operation,
            requested_url,
            &final_url,
            status,
            content_type.as_deref(),
            &body,
        );
        Ok(HttpResponse {
            status,
            content_type,
            body,
            retry_after,
        })
    }

    /// Fetch track using API
    pub fn track(&self, id: i64) -> Result<Option<BeatportTrack>, Error> {
        let url = format!("{API_URL}/tracks/{id}");
        let response = self.send_api_get(&url, "Beatport track")?;

        // Restricted / deleted track
        if matches!(
            response.status,
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND
        ) {
            return Ok(None);
        }
        Ok(Some(parse_json_response(&response, "track")?))
    }

    /// Fetch detailed tracks by ISRC using the catalog API's exact filter.
    pub fn tracks_by_isrc(&self, isrc: &str) -> Result<Vec<BeatportTrack>, Error> {
        let mut url = url::Url::parse(&format!("{API_URL}/tracks/"))?;
        url.query_pairs_mut()
            .append_pair("isrc", isrc.trim())
            .append_pair("per_page", "25");
        let response = self.send_api_get(url.as_str(), "Beatport ISRC lookup")?;
        let response: BeatportPagination<BeatportTrack> =
            parse_json_response(&response, "ISRC lookup")?;
        Ok(response.results)
    }

    /// Fetch release using API
    pub fn release(&self, id: i64) -> Result<BeatportRelease, Error> {
        let url = format!("{API_URL}/releases/{id}");
        let response = self.send_api_get(&url, "Beatport release")?;
        parse_json_response(&response, "release")
    }

    /// Get tracks from release
    pub fn release_tracks(&self, id: i64) -> Result<Vec<BeatportTrack>, Error> {
        let url = format!("{API_URL}/releases/{id}/tracks?per_page=200");
        let response = self.send_api_get(&url, "Beatport release tracks")?;
        let response: BeatportPagination<BeatportTrack> =
            parse_json_response(&response, "release tracks")?;
        Ok(response.results)
    }

    /// Beatport returns 403 if you have more than single () pair
    pub fn clear_search_query(query: &str) -> String {
        let mut open = 0;
        let mut closed = 0;

        query
            .chars()
            .filter(|c| match c {
                '(' if open > 0 => false,
                '(' => {
                    open += 1;
                    true
                }
                ')' if closed > 0 => false,
                ')' => {
                    closed += 1;
                    true
                }
                _ => true,
            })
            .collect()
    }
}

#[derive(Clone)]
struct BeatportToken {
    access_token: String,
    expires_at: u128,
}

#[derive(Debug, Clone, Deserialize)]
struct BeatportAnonSession {
    access_token: String,
    expires_in: u128,
    #[serde(rename = "expirationTime")]
    expiration_time: Option<u128>,
}

struct ParsedSearchPage {
    results: BeatportTrackResults,
    session: Option<BeatportAnonSession>,
}

struct HttpResponse {
    status: StatusCode,
    content_type: Option<String>,
    body: String,
    retry_after: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyKind {
    Empty,
    Json,
    Html,
    Challenge,
    Other,
}

impl BodyKind {
    fn label(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Json => "JSON",
            Self::Html => "HTML",
            Self::Challenge => "challenge/error page",
            Self::Other => "unknown",
        }
    }
}

fn classify_body(body: &str, content_type: Option<&str>) -> BodyKind {
    let trimmed = body.trim_start();
    if trimmed.is_empty() {
        return BodyKind::Empty;
    }
    let lowercase = body.to_ascii_lowercase();
    if lowercase.contains("cf-chl-")
        || lowercase.contains("just a moment...")
        || lowercase.contains("captcha")
        || lowercase.contains("access denied")
    {
        return BodyKind::Challenge;
    }
    if trimmed.starts_with('{')
        || trimmed.starts_with('[')
        || content_type.is_some_and(|value| value.to_ascii_lowercase().contains("json"))
    {
        return BodyKind::Json;
    }
    if trimmed.starts_with('<')
        || content_type.is_some_and(|value| value.to_ascii_lowercase().contains("html"))
    {
        return BodyKind::Html;
    }
    BodyKind::Other
}

fn sanitize_url(value: &str) -> String {
    match url::Url::parse(value) {
        Ok(mut url) => {
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => "<invalid URL>".to_string(),
    }
}

fn sanitized_preview(body: &str) -> String {
    static SECRET_FIELD: OnceLock<Regex> = OnceLock::new();
    static BEARER: OnceLock<Regex> = OnceLock::new();
    let secret_field = SECRET_FIELD.get_or_init(|| {
        Regex::new(r#"(?i)(\"(?:access_token|refresh_token|client_secret)\"\s*:\s*\")[^\"]*"#)
            .unwrap()
    });
    let bearer = BEARER.get_or_init(|| Regex::new(r"(?i)bearer\s+[a-z0-9._~-]+").unwrap());
    let redacted = secret_field.replace_all(body, "$1[REDACTED]");
    let redacted = bearer.replace_all(&redacted, "Bearer [REDACTED]");
    redacted
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(180)
        .collect()
}

fn log_response(
    operation: &str,
    requested_url: &str,
    final_url: &str,
    status: StatusCode,
    content_type: Option<&str>,
    body: &str,
) {
    let kind = classify_body(body, content_type);
    debug!(
        "{operation}: url={}, status={}, redirect={}, content_type={}, response_length={}, preview={:?}, contains_next_data={}, body_kind={}",
        sanitize_url(requested_url),
        status,
        final_url,
        content_type.unwrap_or("<missing>"),
        body.len(),
        sanitized_preview(body),
        body.contains("__NEXT_DATA__"),
        kind.label(),
    );
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retry_delay(retry_after: Option<&str>, attempt: usize) -> Duration {
    let delay = retry_after
        .and_then(|value| {
            value
                .parse::<u64>()
                .ok()
                .map(Duration::from_secs)
                .or_else(|| {
                    DateTime::parse_from_rfc2822(value).ok().map(|date| {
                        let seconds =
                            (date.with_timezone(&Utc) - Utc::now()).num_seconds().max(0) as u64;
                        Duration::from_secs(seconds)
                    })
                })
        })
        .unwrap_or_else(|| Duration::from_secs(2u64.pow(attempt as u32)));
    delay.min(MAX_RETRY_DELAY)
}

fn ensure_success(response: &HttpResponse, operation: &str) -> Result<(), Error> {
    if response.status.is_success() {
        return Ok(());
    }
    Err(anyhow!(
        "Beatport returned HTTP {} during {operation} ({})",
        response.status,
        classify_body(&response.body, response.content_type.as_deref()).label(),
    ))
}

fn parse_json_response<T: for<'de> Deserialize<'de>>(
    response: &HttpResponse,
    operation: &str,
) -> Result<T, Error> {
    ensure_success(response, operation)?;
    match classify_body(&response.body, response.content_type.as_deref()) {
        BodyKind::Empty => {
            return Err(anyhow!(
                "Beatport {operation} endpoint returned an empty response"
            ))
        }
        BodyKind::Html | BodyKind::Challenge => {
            return Err(anyhow!(
                "Beatport returned HTML instead of JSON during {operation}"
            ))
        }
        BodyKind::Other => {
            return Err(anyhow!(
                "Beatport returned an unexpected content type during {operation}"
            ))
        }
        BodyKind::Json => {}
    }
    serde_json::from_str(&response.body)
        .with_context(|| format!("Beatport {operation} returned malformed JSON"))
}

fn parse_search_page(body: &str, content_type: Option<&str>) -> Result<ParsedSearchPage, Error> {
    match classify_body(body, content_type) {
        BodyKind::Empty => return Err(anyhow!("Beatport search returned an empty response")),
        BodyKind::Challenge => {
            return Err(anyhow!("Beatport search returned a challenge/error page"))
        }
        BodyKind::Json => {
            return Err(anyhow!(
                "Beatport returned JSON instead of the search HTML page"
            ))
        }
        BodyKind::Other => {
            return Err(anyhow!(
                "Beatport search returned an unexpected response type"
            ))
        }
        BodyKind::Html => {}
    }

    let document = Html::parse_document(body);
    let selector = Selector::parse("script#__NEXT_DATA__").unwrap();
    let script = document
        .select(&selector)
        .next()
        .ok_or_else(|| anyhow!("Beatport search page no longer contains __NEXT_DATA__"))?
        .text()
        .collect::<String>();
    if script.trim().is_empty() {
        return Err(anyhow!("Beatport search page contains empty __NEXT_DATA__"));
    }
    let value: Value =
        serde_json::from_str(&script).context("Beatport search hydration JSON is malformed")?;
    let page_props = value.pointer("/props/pageProps").ok_or_else(|| {
        anyhow!("Beatport hydration JSON structure has changed: missing pageProps")
    })?;
    let session = page_props
        .get("anonSession")
        .filter(|value| !value.is_null())
        .map(|value| {
            serde_json::from_value(value.clone())
                .context("Beatport anonymous session structure has changed")
        })
        .transpose()?;
    let queries = page_props
        .pointer("/dehydratedState/queries")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Beatport hydration JSON structure has changed: missing queries"))?;
    let search_query = queries
        .iter()
        .find(|query| query.pointer("/queryKey/0").and_then(Value::as_str) == Some("search-tracks"))
        .or_else(|| (queries.len() == 1).then(|| &queries[0]))
        .ok_or_else(|| {
            anyhow!("Beatport hydration JSON structure has changed: missing search-tracks query")
        })?;
    let data = search_query.pointer("/state/data").ok_or_else(|| {
        anyhow!("Beatport hydration JSON structure has changed: missing search data")
    })?;
    let results = serde_json::from_value(data.clone())
        .context("Beatport search result structure has changed")?;
    Ok(ParsedSearchPage { results, session })
}

/// When searching for tracks
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportTrackResults {
    pub data: Vec<BeatportTrackResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportTrackResult {
    pub track_id: i64,
    pub track_name: String,
    pub artists: Option<Vec<BeatportArtist>>,
    pub isrc: Option<String>,
    pub length: Option<u64>,
    pub mix_name: Option<String>,
    pub release: Option<BeatportTrackResultRelease>,
    pub genre: Option<Vec<BeatportTrackResultsGenre>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportTrackResultRelease {
    pub release_id: i64,
    pub release_image_uri: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportTrackResultsGenre {
    pub genre_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportArtist {
    pub artist_id: i64,
    pub artist_name: String,
    pub artist_type_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportTrack {
    pub artists: Vec<BeatportGeneric>,
    pub bpm: Option<i64>,
    pub catalog_number: Option<String>,
    pub exclusive: bool,
    pub genre: BeatportGeneric,
    pub id: i64,
    pub image: Option<BeatportImage>,
    pub isrc: Option<String>,
    pub key: Option<BeatportGeneric>,
    pub length_ms: Option<u64>,
    pub mix_name: String,
    pub name: String,
    pub number: Option<i64>,
    pub publish_date: Option<String>,
    pub release: BeatportRelease,
    pub remixers: Vec<BeatportGeneric>,
    pub slug: String,
    pub sub_genre: Option<BeatportGeneric>,
    pub new_release_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportGeneric {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportImage {
    pub id: i64,
    pub dynamic_uri: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BeatportRelease {
    pub id: i64,
    pub name: String,
    pub label: BeatportGeneric,
    pub image: BeatportImage,
    pub upc: Option<String>,
    pub track_count: Option<u16>,
    pub artists: Option<Vec<BeatportGeneric>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BeatportPagination<T> {
    pub results: Vec<T>,
}

impl BeatportTrackResult {
    pub fn to_track(self, include_version: bool) -> Track {
        Track {
            platform: "beatport".to_string(),
            url: format!(
                "https://www.beatport.com/track/{}/{}",
                self.track_name.to_lowercase().replace(" ", "-"),
                self.track_id
            ),
            title: self.track_name,
            track_id: Some(self.track_id.to_string()),
            artists: self
                .artists
                .unwrap_or(vec![])
                .into_iter()
                .map(|a| a.artist_name)
                .collect(),
            version: match include_version {
                true => self.mix_name,
                false => None,
            },
            duration: Duration::from_millis(self.length.unwrap_or(0)).into(),
            isrc: self.isrc,
            thumbnail: self.release.map(|r| r.release_image_uri).flatten(),
            genres: self
                .genre
                .map(|g| g.into_iter().map(|g| g.genre_name).collect())
                .unwrap_or(vec![]),
            ..Default::default()
        }
    }
}

impl BeatportTrack {
    pub fn to_track(self, art_resolution: u32) -> Track {
        let art = self.get_art(art_resolution);
        let thumbnail = self.get_art(150);

        let mut track = Track {
            platform: "beatport".to_string(),
            title: self.name,
            version: Some(self.mix_name),
            artists: self.artists.into_iter().map(|a| a.name).collect(),
            album: Some(self.release.name),
            key: self
                .key
                .map(|k| k.name.replace(" Major", "").replace(" Minor", "m")),
            bpm: self.bpm,
            genres: vec![self.genre.name],
            styles: match self.sub_genre {
                Some(s) => vec![s.name],
                None => vec![],
            },
            art,
            url: format!("https://www.beatport.com/track/{}/{}", self.slug, self.id),
            label: Some(self.release.label.name),
            catalog_number: self.catalog_number,
            other: vec![(
                FrameName::same("UNIQUEFILEID"),
                vec![format!("https://beatport.com|{}", &self.id)],
            )],
            track_id: Some(self.id.to_string()),
            release_id: Some(self.release.id.to_string()),
            duration: Duration::from_millis(self.length_ms.unwrap_or(0)).into(),
            remixers: self.remixers.into_iter().map(|r| r.name).collect(),
            track_number: self.number.map(|n| TrackNumber::Number(n as i32)),
            isrc: self.isrc,
            release_year: self
                .new_release_date
                .as_ref()
                .map(|d| d.chars().take(4).collect::<String>().parse().ok())
                .flatten(),
            publish_year: self
                .publish_date
                .as_ref()
                .map(|d| d.chars().take(4).collect::<String>().parse().ok())
                .flatten(),
            release_date: self
                .new_release_date
                .as_ref()
                .map_or(None, |d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()),
            publish_date: self
                .publish_date
                .as_ref()
                .map_or(None, |d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()),
            thumbnail,
            ..Default::default()
        };

        // Exclusive
        if self.exclusive {
            track
                .other
                .push((FrameName::same("BEATPORT_EXCLUSIVE"), vec!["1".to_string()]));
        }

        track
    }

    /// Get album art URL
    pub fn get_art(&self, art_resolution: u32) -> Option<String> {
        if self.release.image.dynamic_uri.contains(&INVALID_ART) {
            return None;
        }
        let r = art_resolution.to_string();
        Some(
            self.release
                .image
                .dynamic_uri
                .replace("{w}", &r)
                .replace("{h}", &r)
                .replace("{x}", &r)
                .replace("{y}", &r),
        )
    }
}

// Match track
impl AutotaggerSource for Beatport {
    fn match_track(
        &mut self,
        info: &AudioFileInfo,
        config: &TaggerConfig,
    ) -> Result<Vec<TrackMatch>, Error> {
        // Load custom config
        let custom_config: BeatportConfig = config.get_custom("beatport")?;
        let mut output = vec![];

        // Fetch by ID
        if let Some(id) = info
            .tags
            .get("BEATPORT_TRACK_ID")
            .map(|t| {
                t.first()
                    .map(|id| id.trim().replace("\0", "").parse().ok())
                    .flatten()
            })
            .flatten()
        {
            info!("Fetching by ID: {}", id);
            match self.track(id) {
                Ok(Some(api_track)) => {
                    let track =
                        TrackMatch::new_id(api_track.to_track(custom_config.art_resolution));
                    if !config.fetch_all_results {
                        return Ok(vec![track]);
                    }
                    output.push(track);
                }
                Ok(None) => warn!("Matching by ID failed, track restricted, matching normally"),
                Err(e) => {
                    warn!("Matching by ID failed, matching normally: {e}");
                }
            }
        }

        // Fetch by ISRC
        if let Some(isrc) = info.isrc.as_ref() {
            match self.tracks_by_isrc(isrc) {
                Ok(results) => {
                    if let Some(track) = results.into_iter().find(|track| {
                        track
                            .isrc
                            .as_deref()
                            .is_some_and(|candidate| candidate.eq_ignore_ascii_case(isrc))
                    }) {
                        let track =
                            TrackMatch::new_isrc(track.to_track(custom_config.art_resolution));
                        if !config.fetch_all_results {
                            return Ok(vec![track]);
                        }
                        output.push(track);
                    }
                }
                Err(e) => {
                    warn!("Failed fetching track by ISRC: {e}");
                }
            }
        }

        // Search
        let query = format!(
            "{} {}",
            info.artist()?,
            MatchingUtils::clean_title(info.title()?)
        );
        debug!("BP Query: {}", query);
        for page in 1..custom_config.max_pages + 1 {
            match self.search(&query, page, 50) {
                Ok(res) => {
                    // Match
                    let tracks = res
                        .data
                        .into_iter()
                        .map(|t| t.to_track(true))
                        .collect::<Vec<_>>();

                    // Ignore version match
                    let tracks = if custom_config.ignore_version {
                        let t = tracks
                            .clone()
                            .into_iter()
                            .map(|mut t| {
                                t.version = None;
                                t
                            })
                            .collect();
                        // Copy back versions
                        MatchingUtils::match_track(info, &t, config, true)
                            .into_iter()
                            .map(|mut t| {
                                if let Some(ot) = tracks.iter().find(|ot| ot.url == t.track.url) {
                                    t.track.version = ot.version.to_owned();
                                }
                                t
                            })
                            .collect()
                    } else {
                        MatchingUtils::match_track(info, &tracks, config, true)
                    };

                    // Return
                    output.extend(tracks);
                    if config.fetch_all_results {
                        continue;
                    }
                    return Ok(output);
                }
                Err(e) => {
                    warn!("Beatport search failed, query: {}. {}", query, e);
                    return Ok(output);
                }
            }
        }
        Ok(output)
    }

    fn extend_track(&mut self, track: &mut Track, config: &TaggerConfig) -> Result<(), Error> {
        let custom_config: BeatportConfig = config.get_custom("beatport")?;

        // Extend search results track
        if track.other.is_empty() {
            let id = track.track_id.as_ref().unwrap().parse().unwrap();
            *track = self
                .track(id)?
                .ok_or(anyhow!("Restricted track"))?
                .to_track(custom_config.art_resolution);
        }

        // Ignore extending track
        if !config.tag_enabled(SupportedTag::AlbumArtist)
            && !config.tag_enabled(SupportedTag::TrackTotal)
        {
            return Ok(());
        }

        let release = self.release(
            track
                .release_id
                .as_ref()
                .ok_or(anyhow!("Missing release_id"))?
                .parse()?,
        )?;
        track.track_total = release.track_count;
        track.album_artists = match release.artists {
            Some(a) => a.into_iter().map(|a| a.name).collect(),
            None => vec![],
        };
        Ok(())
    }

    fn get_album(&mut self, id: &str, config: &TaggerConfig) -> Result<Option<Album>, Error> {
        let custom_config: BeatportConfig = config.get_custom("beatport")?;
        let id: i64 = id.trim().parse()?;
        let release = self.release(id)?;
        let tracks = self.release_tracks(id)?;

        let album = Album {
            id: id.to_string(),
            name: release.name,
            tracks: tracks
                .into_iter()
                .map(|t| t.to_track(custom_config.art_resolution))
                .collect(),
        };

        Ok(Some(album))
    }
}

/// For creating Beatport instances
#[derive(Clone)]
pub struct BeatportBuilder {
    access_token: Arc<Mutex<Option<BeatportToken>>>,
}

impl AutotaggerSourceBuilder for BeatportBuilder {
    fn new() -> BeatportBuilder {
        BeatportBuilder {
            access_token: Arc::new(Mutex::new(None)),
        }
    }

    fn get_source(&mut self, _config: &TaggerConfig) -> Result<Box<dyn AutotaggerSource>, Error> {
        Ok(Box::new(Beatport::new(self.access_token.clone())))
    }

    fn info(&self) -> PlatformInfo {
        PlatformInfo {
            id: "beatport".to_string(),
            name: "Beatport".to_string(),
            description: "Overall more specialized in Techno, can match using ISRC".to_string(),
            icon: include_bytes!("../assets/beatport.png"),
            max_threads: 1,
            version: "1.0.0".to_string(),
            requires_auth: false,
            supported_tags: supported_tags!(
                Title,
                Version,
                Artist,
                AlbumArtist,
                Album,
                BPM,
                Genre,
                Style,
                Label,
                URL,
                ReleaseDate,
                PublishDate,
                Key,
                AlbumArt,
                OtherTags,
                TrackId,
                ReleaseId,
                Duration,
                Remixer,
                CatalogNumber,
                TrackTotal,
                ISRC,
                TrackNumber
            ),
            custom_options: PlatformCustomOptions::new()
                // Album art resolution
                .add(
                    "art_resolution",
                    "Album art resolution",
                    PlatformCustomOptionValue::Number {
                        min: 200,
                        max: 1600,
                        step: 100,
                        value: 500,
                    },
                )
                // Max pages to search
                .add_tooltip(
                    "max_pages",
                    "Max pages",
                    "How many pages of search results to scan for tracks",
                    PlatformCustomOptionValue::Number {
                        min: 1,
                        max: 10,
                        step: 1,
                        value: 1,
                    },
                )
                // Ignore version
                .add_tooltip(
                    "ignore_version",
                    "Ignore version when matching",
                    "Ignores (Extended Mix), (Original Mix) and such",
                    PlatformCustomOptionValue::Boolean { value: false },
                ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BeatportConfig {
    pub art_resolution: u32,
    pub max_pages: i32,
    pub ignore_version: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_SEARCH: &str = include_str!("../tests/fixtures/beatport/search_valid.html");
    const EMPTY_SEARCH: &str = include_str!("../tests/fixtures/beatport/search_empty.html");
    const MISSING_HYDRATION: &str =
        include_str!("../tests/fixtures/beatport/search_missing_hydration.html");
    const CHANGED_HYDRATION: &str =
        include_str!("../tests/fixtures/beatport/search_changed_hydration.html");
    const MALFORMED_SEARCH: &str = include_str!("../tests/fixtures/beatport/search_malformed.html");
    const EMPTY_BODY: &str = include_str!("../tests/fixtures/beatport/empty_body.txt");
    const CHALLENGE: &str = include_str!("../tests/fixtures/beatport/challenge.html");
    const HTTP_403: &str = include_str!("../tests/fixtures/beatport/http_403.html");
    const HTTP_429: &str = include_str!("../tests/fixtures/beatport/http_429.json");
    const MALFORMED_JSON: &str = include_str!("../tests/fixtures/beatport/malformed.json");
    const TRACK_GENRE_SUBGENRE: &str =
        include_str!("../tests/fixtures/beatport/track_genre_subgenre.json");
    const TRACK_OPTIONAL_MISSING: &str =
        include_str!("../tests/fixtures/beatport/track_optional_missing.json");

    fn response(status: StatusCode, content_type: &str, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            content_type: Some(content_type.to_string()),
            body: body.to_string(),
            retry_after: None,
        }
    }

    #[test]
    fn parses_valid_search_query_by_key() {
        let parsed = parse_search_page(VALID_SEARCH, Some("text/html; charset=utf-8")).unwrap();
        assert_eq!(parsed.results.data.len(), 1);
        assert_eq!(parsed.results.data[0].track_id, 123456);
        assert_eq!(
            parsed.results.data[0].genre.as_ref().unwrap()[0].genre_name,
            "House"
        );
        assert_eq!(parsed.session.unwrap().access_token, "fixture-token");
    }

    #[test]
    fn parses_no_search_results() {
        let parsed = parse_search_page(EMPTY_SEARCH, Some("text/html")).unwrap();
        assert!(parsed.results.data.is_empty());
    }

    #[test]
    fn reports_missing_and_changed_hydration() {
        let missing = parse_search_page(MISSING_HYDRATION, Some("text/html"))
            .err()
            .unwrap()
            .to_string();
        assert!(missing.contains("no longer contains __NEXT_DATA__"));
        let changed = parse_search_page(CHANGED_HYDRATION, Some("text/html"))
            .err()
            .unwrap()
            .to_string();
        assert!(changed.contains("missing search-tracks query"));
    }

    #[test]
    fn reports_empty_and_challenge_responses() {
        assert!(parse_search_page(EMPTY_BODY, None)
            .err()
            .unwrap()
            .to_string()
            .contains("empty response"));
        assert!(parse_search_page(CHALLENGE, Some("text/html"))
            .err()
            .unwrap()
            .to_string()
            .contains("challenge/error page"));
    }

    #[test]
    fn reports_http_403_without_retrying() {
        let response = response(StatusCode::FORBIDDEN, "text/html", HTTP_403);
        assert!(!is_retryable(response.status));
        assert!(ensure_success(&response, "search")
            .unwrap_err()
            .to_string()
            .contains("HTTP 403"));
    }

    #[test]
    fn retries_http_429_and_respects_retry_after() {
        let response = response(StatusCode::TOO_MANY_REQUESTS, "application/json", HTTP_429);
        assert!(is_retryable(response.status));
        assert!(ensure_success(&response, "search")
            .unwrap_err()
            .to_string()
            .contains("HTTP 429"));
        assert_eq!(retry_delay(Some("7"), 0), Duration::from_secs(7));
    }

    #[test]
    fn reports_malformed_json() {
        let search_error = parse_search_page(MALFORMED_SEARCH, Some("text/html"))
            .err()
            .unwrap()
            .to_string();
        assert!(search_error.contains("hydration JSON is malformed"));
        let response = response(StatusCode::OK, "application/json", MALFORMED_JSON);
        let api_error =
            parse_json_response::<BeatportPagination<BeatportTrack>>(&response, "tracks")
                .unwrap_err()
                .to_string();
        assert!(api_error.contains("malformed JSON"));
    }

    #[test]
    fn parses_track_genre_and_subgenre() {
        let track: BeatportTrack = serde_json::from_str(TRACK_GENRE_SUBGENRE).unwrap();
        let track = track.to_track(500);
        assert_eq!(track.genres, vec!["House"]);
        assert_eq!(track.styles, vec!["Deep House"]);
        assert_eq!(track.label.as_deref(), Some("Example Label"));
        assert_eq!(track.bpm, Some(128));
    }

    #[test]
    fn parses_track_without_optional_fields() {
        let track: BeatportTrack = serde_json::from_str(TRACK_OPTIONAL_MISSING).unwrap();
        assert!(track.bpm.is_none());
        assert!(track.sub_genre.is_none());
        assert!(track.isrc.is_none());
        assert!(track.length_ms.is_none());
    }

    #[test]
    fn diagnostic_preview_redacts_secrets() {
        let preview = sanitized_preview(r#"{"access_token":"secret-value","message":"ok"}"#);
        assert!(!preview.contains("secret-value"));
        assert!(preview.contains("[REDACTED]"));
    }

    #[test]
    #[ignore = "manual live Beatport integration test"]
    fn live_search_and_track_details() {
        let beatport = Beatport::new(Arc::new(Mutex::new(None)));
        let cases = [
            ("deadmau5 strobe", "strobe"),
            ("Daft Punk One More Time", "one more time"),
            ("CamelPhat Elderbrook Cola", "cola"),
        ];

        for (query, expected_title) in cases {
            let results = beatport.search(query, 1, 25).unwrap();
            let result = results
                .data
                .iter()
                .find(|track| {
                    track
                        .track_name
                        .to_ascii_lowercase()
                        .contains(expected_title)
                })
                .unwrap_or_else(|| panic!("Beatport did not return {expected_title}"));
            let track = beatport
                .track(result.track_id)
                .unwrap()
                .expect("Track is restricted or missing");
            assert_eq!(track.id, result.track_id);
            assert!(!track.artists.is_empty());
            assert!(!track.genre.name.is_empty());
            assert!(!track.release.label.name.is_empty());
            assert!(track.length_ms.unwrap_or_default() > 0);

            let expected_subgenre = track.sub_genre.as_ref().map(|genre| genre.name.clone());
            let converted = track.clone().to_track(500);
            assert_eq!(converted.genres, vec![track.genre.name.clone()]);
            assert_eq!(converted.styles.first(), expected_subgenre.as_ref());
            beatport.release(track.release.id).unwrap();

            if let Some(isrc) = result.isrc.as_deref() {
                let isrc_results = beatport.tracks_by_isrc(isrc).unwrap();
                assert!(isrc_results
                    .iter()
                    .filter_map(|candidate| candidate.isrc.as_deref())
                    .any(|candidate_isrc| candidate_isrc.eq_ignore_ascii_case(isrc)));
            }
        }

        assert!(beatport.track(i64::MAX).unwrap().is_none());
    }

    #[test]
    #[ignore = "manual live Beatport autotagger integration test"]
    fn live_autotagger_matching_paths() {
        use onetagger_tag::AudioFileFormat;
        use onetagger_tagger::FileTaggedStatus;
        use std::collections::HashMap;
        use std::path::PathBuf;

        let builder = BeatportBuilder::new();
        let mut config = TaggerConfig::default();
        config.custom.0.insert(
            "beatport".to_string(),
            builder.info().custom_options.get_defaults(),
        );
        let mut beatport = Beatport::new(builder.access_token.clone());
        let base_info = AudioFileInfo {
            title: Some("Strobe".to_string()),
            artists: vec!["deadmau5".to_string()],
            format: AudioFileFormat::MP3,
            path: PathBuf::from("disposable-test.mp3"),
            isrc: None,
            duration: None,
            track_number: None,
            tagged: FileTaggedStatus::Untagged,
            tags: HashMap::new(),
        };

        let title_matches = beatport.match_track(&base_info, &config).unwrap();
        assert!(!title_matches.is_empty());
        assert!(title_matches[0]
            .track
            .title
            .to_ascii_lowercase()
            .contains("strobe"));

        let search = beatport.search("deadmau5 strobe", 1, 25).unwrap();
        let result = search
            .data
            .iter()
            .find(|track| track.track_name.to_ascii_lowercase().contains("strobe"))
            .unwrap();
        let isrc = result
            .isrc
            .clone()
            .expect("Strobe search result has no ISRC");
        let mut isrc_info = base_info.clone();
        isrc_info.isrc = Some(isrc);
        let isrc_matches = beatport.match_track(&isrc_info, &config).unwrap();
        assert!(!isrc_matches.is_empty());

        let mut id_info = base_info;
        id_info.tags.insert(
            "BEATPORT_TRACK_ID".to_string(),
            vec![result.track_id.to_string()],
        );
        let id_matches = beatport.match_track(&id_info, &config).unwrap();
        assert_eq!(
            id_matches[0].track.track_id.as_deref(),
            Some(result.track_id.to_string()).as_deref()
        );
    }

    #[test]
    #[ignore = "manual live Beatport album integration test"]
    fn live_album() {
        let mut builder = BeatportBuilder::new();
        let mut config = TaggerConfig::default();
        let custom_config = builder.info().custom_options.get_defaults();
        config
            .custom
            .0
            .insert("beatport".to_string(), custom_config);
        let mut beatport = builder.get_source(&config).unwrap();
        beatport.get_album("2174307", &config).unwrap();
    }
}
