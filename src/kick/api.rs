//! Clients for the Kick HTTP APIs.
//!
//! * The official public API (`api.kick.com`, authenticated with an app access token) is used to
//!   translate between channel slugs and user ids, like Helix is used for Twitch.
//! * Kick's website API (`kick.com/api/v2`) is used to find the chatroom id of a channel, which
//!   is needed to subscribe to its chat. The official API does not expose it. The website API is
//!   protected by Cloudflare, so a request which fails is repeated through `curl`, whose TLS
//!   fingerprint is usually let through.

use reqwest::{header, Client, StatusCode};
use serde_json::Value;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::{process::Command, sync::Mutex, time::sleep};
use tracing::{debug, info, warn};

const API_BASE_URL: &str = "https://api.kick.com";
const OAUTH_BASE_URL: &str = "https://id.kick.com";
const WEB_BASE_URL: &str = "https://kick.com";
const API_USER_AGENT: &str = concat!("rustlog-kick/", env!("CARGO_PKG_VERSION"));
/// The website API only answers requests which look like they come from a browser
const BROWSER_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/139.0.0.0 Safari/537.36";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Tokens are replaced this long before they expire
const TOKEN_EXPIRY_MARGIN: Duration = Duration::from_secs(60);
const DEFAULT_TOKEN_LIFETIME_SECONDS: u64 = 3600;
/// Keeps absurd values from the token endpoint from overflowing the expiry time
const MAX_TOKEN_LIFETIME_SECONDS: u64 = 365 * 24 * 3600;
const DEFAULT_RATE_LIMIT_WAIT_SECONDS: u64 = 1;
const MAX_RATE_LIMIT_WAIT_SECONDS: u64 = 30;
const MAX_ERROR_BODY_LENGTH: usize = 300;

/// The maximum number of ids or slugs the channels endpoint accepts per request
pub const MAX_LOOKUP_BATCH: usize = 50;
/// The longest channel slug the official API accepts
pub const MAX_SLUG_LENGTH: usize = 25;
const MAX_WEB_SLUG_LENGTH: usize = 64;

#[derive(Error, Debug)]
pub enum KickError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{url} returned status {status}: {body}")]
    Status {
        url: String,
        status: u16,
        body: String,
    },
    #[error("Could not authenticate with the Kick API: {0}")]
    Auth(String),
    #[error("Unexpected response from Kick: {0}")]
    Response(String),
    #[error("{0}")]
    Other(String),
}

/// A channel as returned by the official API
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KickChannel {
    /// User id of the broadcaster
    pub user_id: String,
    pub slug: String,
}

/// A channel as returned by Kick's website API
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebChannel {
    /// User id of the broadcaster
    pub user_id: String,
    pub slug: String,
    /// The id the chat of the channel is published under. Not the same as the channel or user id.
    pub chatroom_id: u64,
}

struct AppToken {
    value: String,
    expires_at: Instant,
}

pub struct KickApi {
    http: Client,
    client_id: String,
    client_secret: String,
    token: Mutex<Option<AppToken>>,
}

impl KickApi {
    pub fn new(client_id: String, client_secret: String) -> Result<Self, KickError> {
        let http = Client::builder()
            .user_agent(API_USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;

        Ok(Self {
            http,
            client_id,
            client_secret,
            token: Mutex::new(None),
        })
    }

    /// Requests an app access token. Fails if the configured credentials are not valid.
    pub async fn authenticate(&self) -> Result<(), KickError> {
        // Nothing is cached yet when this runs at startup
        self.access_token(None).await.map(|_| ())
    }

    /// Returns a valid app access token, requesting a new one if needed.
    ///
    /// `rejected` is a token the API refused. It is replaced even if it has not expired, unless
    /// another caller already replaced it, so many requests failing with the same stale token
    /// cause a single refresh.
    async fn access_token(&self, rejected: Option<&str>) -> Result<String, KickError> {
        let mut guard = self.token.lock().await;

        if let Some(token) = guard.as_ref() {
            if Instant::now() < token.expires_at && rejected != Some(token.value.as_str()) {
                return Ok(token.value.clone());
            }
        }

        let response = self
            .http
            .post(format!("{OAUTH_BASE_URL}/oauth/token"))
            .form(&[
                ("grant_type", "client_credentials"),
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
            ])
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(KickError::Auth(format!(
                "the token endpoint returned status {status}: {}",
                truncate(&body)
            )));
        }

        let (access_token, lifetime) = parse_token_response(&body)?;
        let lifetime = Duration::from_secs(lifetime.min(MAX_TOKEN_LIFETIME_SECONDS));
        info!(
            "Generated new app access token (valid for {}s)",
            lifetime.as_secs()
        );

        // Replace the token a little before it expires, but not so early that a token with a
        // short lifetime counts as expired right away
        let margin = TOKEN_EXPIRY_MARGIN.min(lifetime / 2);
        *guard = Some(AppToken {
            value: access_token.clone(),
            expires_at: Instant::now() + lifetime - margin,
        });

        Ok(access_token)
    }

    /// Looks up channels by slug. Slugs which do not exist are missing from the result.
    pub async fn channels_by_slugs(&self, slugs: &[String]) -> Result<Vec<KickChannel>, KickError> {
        self.lookup_channels("slug", slugs).await
    }

    /// Looks up channels by the user id of their broadcaster.
    /// Ids which do not exist are missing from the result.
    pub async fn channels_by_user_ids(
        &self,
        user_ids: &[String],
    ) -> Result<Vec<KickChannel>, KickError> {
        self.lookup_channels("broadcaster_user_id", user_ids).await
    }

    async fn lookup_channels(
        &self,
        key: &str,
        values: &[String],
    ) -> Result<Vec<KickChannel>, KickError> {
        let mut channels = Vec::with_capacity(values.len());

        // There are no chunks if the slice is empty, so there is no empty request made
        for chunk in values.chunks(MAX_LOOKUP_BATCH) {
            let query: Vec<(&str, &str)> =
                chunk.iter().map(|value| (key, value.as_str())).collect();

            match self.api_get("/public/v1/channels", &query).await {
                Ok(body) => channels.extend(parse_channels(&body)),
                // A single invalid value makes Kick reject the whole request,
                // so ask for the values one by one to still get the valid ones
                Err(KickError::Status { status: 400, .. }) if chunk.len() > 1 => {
                    for value in chunk {
                        let query = [(key, value.as_str())];
                        match self.api_get("/public/v1/channels", &query).await {
                            Ok(body) => channels.extend(parse_channels(&body)),
                            Err(KickError::Status { status: 400, .. }) => {
                                debug!("Kick rejected the {key} {value:?}");
                            }
                            Err(err) => return Err(err),
                        }
                    }
                }
                Err(err) => return Err(err),
            }
        }

        Ok(channels)
    }

    async fn api_get(&self, path: &str, query: &[(&str, &str)]) -> Result<Value, KickError> {
        let url = format!("{API_BASE_URL}{path}");
        let mut rejected_token: Option<String> = None;
        let mut waited_for_rate_limit = false;

        loop {
            let token = self.access_token(rejected_token.as_deref()).await?;
            let response = self
                .http
                .get(&url)
                .bearer_auth(&token)
                .query(query)
                .send()
                .await?;
            let status = response.status();

            if status == StatusCode::UNAUTHORIZED && rejected_token.is_none() {
                warn!("The Kick API rejected the access token, requesting a new one");
                rejected_token = Some(token);
                continue;
            }

            if status == StatusCode::TOO_MANY_REQUESTS && !waited_for_rate_limit {
                let wait_seconds = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()?.trim().parse::<u64>().ok())
                    .unwrap_or(DEFAULT_RATE_LIMIT_WAIT_SECONDS)
                    .min(MAX_RATE_LIMIT_WAIT_SECONDS);
                drop(response);

                warn!("The Kick API is rate limiting requests, trying again in {wait_seconds}s");
                waited_for_rate_limit = true;
                sleep(Duration::from_secs(wait_seconds)).await;
                continue;
            }

            let body = response.text().await?;
            if status == StatusCode::UNAUTHORIZED {
                return Err(KickError::Auth(format!(
                    "{url} rejected a new access token: {}",
                    truncate(&body)
                )));
            }
            if !status.is_success() {
                return Err(KickError::Status {
                    url,
                    status: status.as_u16(),
                    body: truncate(&body),
                });
            }

            return serde_json::from_str(&body)
                .map_err(|err| KickError::Response(format!("{url}: {err}")));
        }
    }

    /// Fetches a channel from Kick's website API, which includes the chatroom id
    pub async fn web_channel(&self, slug: &str) -> Result<WebChannel, KickError> {
        if !is_valid_web_slug(slug) {
            return Err(KickError::Other(format!("invalid channel slug {slug:?}")));
        }
        let url = format!("{WEB_BASE_URL}/api/v2/channels/{slug}");

        let body = match self.web_get(&url).await {
            Ok(body) => body,
            // The channel does not exist, curl would not find it either
            Err(err @ KickError::Status { status: 404, .. }) => return Err(err),
            Err(err) => {
                debug!("Request to {url} failed ({err}), retrying with curl");
                fetch_with_curl(&url).await.map_err(|curl_err| {
                    KickError::Other(format!("{err}; trying again with curl: {curl_err}"))
                })?
            }
        };

        parse_web_channel(&body)
    }

    async fn web_get(&self, url: &str) -> Result<Value, KickError> {
        let response = self
            .http
            .get(url)
            .header(header::USER_AGENT, BROWSER_USER_AGENT)
            .header(header::ACCEPT, "application/json")
            .header(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9")
            .send()
            .await?;
        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(KickError::Status {
                url: url.to_owned(),
                status: status.as_u16(),
                body: truncate(&body),
            });
        }

        serde_json::from_str(&body).map_err(|err| KickError::Response(format!("{url}: {err}")))
    }
}

/// Fetches a JSON document by running the `curl` binary
async fn fetch_with_curl(url: &str) -> Result<Value, KickError> {
    let output = Command::new("curl")
        .args([
            // Has to be the first argument. Ignores the user's .curlrc, which could change the
            // output in ways this function does not expect.
            "--disable",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "20",
            "--user-agent",
            BROWSER_USER_AGENT,
            "--header",
            "Accept: application/json",
            "--write-out",
            "\n%{http_code}",
            url,
        ])
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| KickError::Other(format!("could not run curl: {err}")))?;

    if !output.status.success() {
        return Err(KickError::Other(format!(
            "curl failed ({}): {}",
            output.status,
            truncate(&String::from_utf8_lossy(&output.stderr))
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (body, status) = stdout
        .rsplit_once('\n')
        .ok_or_else(|| KickError::Response("curl did not print a status code".to_owned()))?;
    let status = status.trim();
    if status != "200" {
        return Err(KickError::Status {
            url: url.to_owned(),
            status: status.parse().unwrap_or(0),
            body: truncate(body),
        });
    }

    serde_json::from_str(body).map_err(|err| KickError::Response(format!("{url}: {err}")))
}

/// Lowercases and trims a channel name the way slugs are stored
pub fn normalize_slug(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Whether the official API can be asked for this slug
pub fn is_valid_slug(slug: &str) -> bool {
    !slug.is_empty() && slug.len() <= MAX_SLUG_LENGTH && is_slug_charset(slug)
}

fn is_valid_web_slug(slug: &str) -> bool {
    !slug.is_empty() && slug.len() <= MAX_WEB_SLUG_LENGTH && is_slug_charset(slug)
}

fn is_slug_charset(slug: &str) -> bool {
    slug.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn truncate(body: &str) -> String {
    body.chars().take(MAX_ERROR_BODY_LENGTH).collect()
}

/// Returns the access token and its lifetime in seconds
fn parse_token_response(body: &str) -> Result<(String, u64), KickError> {
    let response: Value = serde_json::from_str(body)
        .map_err(|err| KickError::Auth(format!("invalid token response: {err}")))?;

    let access_token = response
        .get("access_token")
        .and_then(|token| token.as_str())
        .filter(|token| !token.is_empty())
        .ok_or_else(|| KickError::Auth("the token response has no access token".to_owned()))?
        .to_owned();

    // `expires_in` is documented without a type, accept numbers and numeric strings
    let lifetime = response
        .get("expires_in")
        .and_then(|lifetime| {
            lifetime
                .as_u64()
                .or_else(|| lifetime.as_str().and_then(|lifetime| lifetime.parse().ok()))
        })
        .unwrap_or(DEFAULT_TOKEN_LIFETIME_SECONDS);

    Ok((access_token, lifetime))
}

fn parse_channels(body: &Value) -> Vec<KickChannel> {
    body.get("data")
        .and_then(|data| data.as_array())
        .map(|channels| {
            channels
                .iter()
                .filter_map(|channel| {
                    let user_id = value_to_id(channel.get("broadcaster_user_id"))?;
                    let slug = channel.get("slug")?.as_str()?.to_owned();
                    Some(KickChannel { user_id, slug })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_web_channel(body: &Value) -> Result<WebChannel, KickError> {
    let user_id = value_to_id(body.get("user_id"))
        .ok_or_else(|| KickError::Response("the channel has no user_id".to_owned()))?;
    let slug = body
        .get("slug")
        .and_then(|slug| slug.as_str())
        .unwrap_or_default()
        .to_owned();
    let chatroom_id = body
        .get("chatroom")
        .and_then(|chatroom| chatroom.get("id"))
        .and_then(|id| id.as_u64())
        .ok_or_else(|| KickError::Response("the channel has no chatroom id".to_owned()))?;

    Ok(WebChannel {
        user_id,
        slug,
        chatroom_id,
    })
}

fn value_to_id(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::Number(id) => Some(id.to_string()),
        Value::String(id) if !id.is_empty() => Some(id.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn slug_validation() {
        assert!(is_valid_slug("xqc"));
        assert!(is_valid_slug("some-user_2"));
        assert!(is_valid_slug(&"a".repeat(MAX_SLUG_LENGTH)));
        assert!(!is_valid_slug(&"a".repeat(MAX_SLUG_LENGTH + 1)));
        assert!(!is_valid_slug(""));
        assert!(!is_valid_slug("with space"));
        assert!(!is_valid_slug("../etc"));
        assert!(!is_valid_slug("name?x=1"));

        // the website API accepts longer names
        assert!(is_valid_web_slug(&"a".repeat(MAX_SLUG_LENGTH + 1)));
        assert!(!is_valid_web_slug("a/b"));
    }

    #[test]
    fn slugs_are_normalized() {
        assert_eq!(normalize_slug("  XQc "), "xqc");
    }

    #[test]
    fn parses_token_responses() {
        assert_eq!(
            parse_token_response(r#"{"access_token":"abc","token_type":"Bearer","expires_in":3600}"#)
                .unwrap(),
            ("abc".to_owned(), 3600)
        );
        assert_eq!(
            parse_token_response(r#"{"access_token":"abc","expires_in":"120"}"#).unwrap(),
            ("abc".to_owned(), 120)
        );
        assert_eq!(
            parse_token_response(r#"{"access_token":"abc"}"#).unwrap(),
            ("abc".to_owned(), DEFAULT_TOKEN_LIFETIME_SECONDS)
        );
        assert!(parse_token_response(r#"{"error":"invalid_client"}"#).is_err());
        assert!(parse_token_response(r#"{"access_token":""}"#).is_err());
        assert!(parse_token_response("<html>").is_err());
    }

    #[test]
    fn parses_official_channel_lists() {
        let body = json!({
            "data": [
                {"broadcaster_user_id": 676, "slug": "xqc", "stream_title": "hi", "stream": {"is_live": true}},
                {"broadcaster_user_id": 7183419, "slug": "amouranth"},
                {"slug": "no-id"},
                {"broadcaster_user_id": 1}
            ],
            "message": "OK"
        });

        assert_eq!(
            parse_channels(&body),
            vec![
                KickChannel {
                    user_id: "676".to_owned(),
                    slug: "xqc".to_owned()
                },
                KickChannel {
                    user_id: "7183419".to_owned(),
                    slug: "amouranth".to_owned()
                },
            ]
        );

        assert_eq!(parse_channels(&json!({"data": [], "message": "OK"})), vec![]);
        assert_eq!(parse_channels(&json!({"data": null})), vec![]);
        assert_eq!(parse_channels(&json!({})), vec![]);
    }

    #[test]
    fn parses_website_channels() {
        // chatroom id and channel id differ for many channels
        let body = json!({
            "id": 7088698,
            "user_id": 7183419,
            "slug": "amouranth",
            "user": {"id": 7183419, "username": "Amouranth"},
            "chatroom": {"id": 7022952, "channel_id": 7088698, "chat_mode": "public"}
        });

        assert_eq!(
            parse_web_channel(&body).unwrap(),
            WebChannel {
                user_id: "7183419".to_owned(),
                slug: "amouranth".to_owned(),
                chatroom_id: 7022952,
            }
        );

        assert!(parse_web_channel(&json!({"user_id": 1, "slug": "x"})).is_err());
        assert!(parse_web_channel(&json!({"slug": "x", "chatroom": {"id": 1}})).is_err());
    }

    #[test]
    fn long_error_bodies_are_truncated() {
        assert_eq!(truncate(&"x".repeat(1000)).len(), MAX_ERROR_BODY_LENGTH);
        assert_eq!(truncate("short"), "short");
    }
}
