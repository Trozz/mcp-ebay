//! eBay's OAuth: the sign-in page, the code exchange and refreshes at `/identity/v1/oauth2/token`,
//! each signed-in person's access tokens, and this app's own application token.
//!
//! People sign in through eBay (see [`crate::verifier`]); the credential sealed into their MCP
//! tokens is their eBay **refresh token**, which lasts 18 months and is never replaced by a
//! refresh. Tools turn it into a user access token (two hours) here: access tokens are cached in
//! memory per person and refreshed when they run out, one refresh at a time per person.
//!
//! Searching eBay and reading categories needs no person: those calls use an application token
//! (client credentials grant), shared by everyone and refreshed the same way.

use crate::error::EbayError;
use mcp_server_core::credential::Credential;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

/// An access token is refreshed this long before eBay says it expires.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(120);
/// Sign-ins whose access tokens are remembered at once; idle ones are dropped first.
const MAX_SLOTS: usize = 10_000;
const MAX_ERROR_DETAIL: usize = 200;

/// The scope of application tokens: public data (search, categories, conditions).
pub const APP_SCOPE: &str = "https://api.ebay.com/oauth/api_scope";

/// What a signed-in person grants this app: selling (inventory, policies, orders) and who they
/// are. Part of the sign-in version: changing it signs everyone out, as their grants no longer
/// match.
pub const USER_SCOPES: &[&str] = &[
    "https://api.ebay.com/oauth/api_scope",
    "https://api.ebay.com/oauth/api_scope/sell.inventory",
    "https://api.ebay.com/oauth/api_scope/sell.account",
    "https://api.ebay.com/oauth/api_scope/sell.fulfillment",
    "https://api.ebay.com/oauth/api_scope/commerce.identity.readonly",
];

pub fn user_scope() -> String {
    USER_SCOPES.join(" ")
}

/// The eBay app (keyset) this server signs people in with.
pub struct OAuthApp {
    http: reqwest::Client,
    /// The API base: the token endpoint is below it.
    api_url: Url,
    auth_url: Url,
    client_id: String,
    client_secret: SecretString,
    runame: String,
}

impl std::fmt::Debug for OAuthApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthApp")
            .field("api_url", &self.api_url.as_str())
            .field("client_id", &self.client_id)
            .field("runame", &self.runame)
            .finish_non_exhaustive()
    }
}

/// What the token endpoint granted.
pub struct TokenGrant {
    pub access_token: SecretString,
    pub expires_in: Duration,
    /// Granted by the code exchange only: refreshes keep the refresh token as it was.
    pub refresh_token: Option<SecretString>,
}

/// Why the token endpoint granted nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum TokenError {
    /// The code or refresh token is not (or no longer) valid, with eBay's reason.
    Invalid(String),
    /// eBay refused the app's own credentials (`EBAY_CLIENT_ID`/`SECRET`).
    BadClient,
    RateLimited(Option<u64>),
    Unavailable(String),
}

#[derive(Deserialize)]
struct TokenBody {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
}

impl OAuthApp {
    pub fn new(
        http: reqwest::Client,
        api_url: Url,
        auth_url: Url,
        client_id: String,
        client_secret: SecretString,
        runame: String,
    ) -> Self {
        OAuthApp {
            http,
            api_url,
            auth_url,
            client_id,
            client_secret,
            runame,
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn runame(&self) -> &str {
        &self.runame
    }

    fn token_url(&self) -> Url {
        let mut url = self.api_url.clone();
        url.path_segments_mut()
            .expect("an http(s) URL has path segments")
            .pop_if_empty()
            .extend(["identity", "v1", "oauth2", "token"]);
        url
    }

    /// eBay's page where the person signs in and grants this app access. eBay sends them back to
    /// the auth accepted URL of the RuName, which must be this server's `/callback`; the RuName,
    /// not that URL, is the `redirect_uri`.
    pub fn authorize_url(&self, state: &str) -> String {
        let mut url = self.auth_url.clone();
        url.query_pairs_mut()
            .append_pair("client_id", &self.client_id)
            .append_pair("redirect_uri", &self.runame)
            .append_pair("response_type", "code")
            .append_pair("scope", &user_scope())
            .append_pair("state", state);
        url.into()
    }

    /// Exchanges an authorization code.
    pub async fn exchange(&self, code: &str) -> Result<TokenGrant, TokenError> {
        self.token_request(
            "authorization_code",
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.runame),
            ],
        )
        .await
    }

    /// A new user access token for `refresh_token`.
    pub async fn refresh(&self, refresh_token: &str) -> Result<TokenGrant, TokenError> {
        self.token_request(
            "refresh_token",
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("scope", &user_scope()),
            ],
        )
        .await
    }

    /// An application token, for public data.
    pub async fn application_token(&self) -> Result<TokenGrant, TokenError> {
        self.token_request(
            "client_credentials",
            &[("grant_type", "client_credentials"), ("scope", APP_SCOPE)],
        )
        .await
    }

    async fn token_request(
        &self,
        grant: &'static str,
        form: &[(&str, &str)],
    ) -> Result<TokenGrant, TokenError> {
        let started = Instant::now();
        let result = self
            .http
            .post(self.token_url())
            .header(reqwest::header::ACCEPT, "application/json")
            .basic_auth(&self.client_id, Some(self.client_secret.expose_secret()))
            .form(form)
            .send()
            .await;
        let status = result.as_ref().ok().map(|r| r.status().as_u16());
        metrics::counter!("ebay_token_requests_total", "grant" => grant, "status" => status.map(|s| s.to_string()).unwrap_or_else(|| "error".into()))
            .increment(1);
        tracing::debug!(
            grant,
            status,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "ebay token request"
        );
        let response = result.map_err(|e| {
            TokenError::Unavailable(if e.is_timeout() {
                "eBay did not answer in time".into()
            } else {
                "eBay could not be reached".into()
            })
        })?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let text = response.text().await.unwrap_or_default();
        if status.is_success() {
            let body: TokenBody = serde_json::from_str(&text).map_err(|_| {
                TokenError::Unavailable("eBay's token answer was not understood".into())
            })?;
            if body.access_token.is_empty() {
                return Err(TokenError::Unavailable(
                    "eBay granted no access token".into(),
                ));
            }
            return Ok(TokenGrant {
                access_token: body.access_token.into(),
                expires_in: Duration::from_secs(body.expires_in.unwrap_or(7200)),
                refresh_token: body
                    .refresh_token
                    .filter(|t| !t.is_empty())
                    .map(SecretString::from),
            });
        }
        let detail = token_error_detail(&text);
        let code = detail.as_deref().and_then(|d| d.split(':').next());
        match status.as_u16() {
            429 => Err(TokenError::RateLimited(retry_after)),
            _ if code == Some("invalid_client") => Err(TokenError::BadClient),
            401 => Err(TokenError::BadClient),
            400 | 403 => Err(TokenError::Invalid(
                detail.unwrap_or_else(|| "invalid_grant".into()),
            )),
            code => Err(TokenError::Unavailable(format!(
                "eBay answered HTTP {code}"
            ))),
        }
    }
}

/// `error` (and `error_description`) of an OAuth error body.
fn token_error_detail(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = v.get("error")?.as_str()?;
    let mut out = error.to_string();
    if let Some(d) = v.get("error_description").and_then(|d| d.as_str()) {
        out.push_str(": ");
        out.push_str(d);
    }
    Some(out.chars().take(MAX_ERROR_DETAIL).collect())
}

/// How a failed refresh is reported to a tool.
fn refresh_failure(e: TokenError, what: &str) -> EbayError {
    match e {
        TokenError::Invalid(_) => EbayError::Auth,
        TokenError::BadClient => {
            tracing::error!(
                "eBay refused this server's app credentials: check EBAY_CLIENT_ID and EBAY_CLIENT_SECRET"
            );
            EbayError::Internal(
                "eBay refused this server's app credentials; the server's operator needs to check EBAY_CLIENT_ID and EBAY_CLIENT_SECRET".into(),
            )
        }
        TokenError::RateLimited(retry_after_secs) => EbayError::RateLimited { retry_after_secs },
        TokenError::Unavailable(why) => {
            EbayError::Upstream(format!("getting an eBay {what} failed: {why}"))
        }
    }
}

/// One cached access token and when it stops being used.
type Cached = Option<(SecretString, Instant)>;

fn fresh(cached: &Cached) -> Option<SecretString> {
    cached
        .as_ref()
        .filter(|(_, until)| Instant::now() < *until)
        .map(|(t, _)| t.clone())
}

fn until(grant: &TokenGrant) -> Instant {
    Instant::now() + grant.expires_in.saturating_sub(EXPIRY_MARGIN)
}

/// One sign-in's access token, locked while it is refreshed.
type SharedSlot = Arc<tokio::sync::Mutex<Cached>>;

/// Every signed-in person's access tokens, keyed by the fingerprint of the refresh token sealed
/// into their MCP tokens, and the application token.
pub struct Tokens {
    app: Arc<OAuthApp>,
    slots: Mutex<HashMap<[u8; 32], (SharedSlot, Instant)>>,
    app_token: tokio::sync::Mutex<Cached>,
}

impl Tokens {
    pub fn new(app: Arc<OAuthApp>) -> Self {
        Tokens {
            app,
            slots: Mutex::new(HashMap::new()),
            app_token: tokio::sync::Mutex::new(None),
        }
    }

    pub fn app(&self) -> &OAuthApp {
        &self.app
    }

    fn slot(&self, credential: &Credential) -> SharedSlot {
        let fp = credential.fingerprint();
        let now = Instant::now();
        let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((slot, used)) = slots.get_mut(&fp) {
            *used = now;
            return slot.clone();
        }
        if slots.len() >= MAX_SLOTS {
            // Drop the least recently used half; their access tokens are fetched again if needed.
            let mut by_age: Vec<([u8; 32], Instant)> =
                slots.iter().map(|(k, (_, used))| (*k, *used)).collect();
            by_age.sort_by_key(|(_, used)| *used);
            for (k, _) in by_age.into_iter().take(MAX_SLOTS / 2) {
                slots.remove(&k);
            }
        }
        let slot = Arc::new(tokio::sync::Mutex::new(None));
        slots.insert(fp, (slot.clone(), now));
        slot
    }

    /// Remembers the access token eBay granted with `refresh_token` at sign-in, so the first tool
    /// call does not refresh.
    pub fn seed(&self, refresh_token: &str, grant: &TokenGrant) {
        let slot = self.slot(&Credential::new(refresh_token));
        if let Ok(mut s) = slot.try_lock() {
            *s = Some((grant.access_token.clone(), until(grant)));
        };
    }

    /// A user access token for the person whose sealed refresh token is `credential`.
    pub async fn access_token(&self, credential: &Credential) -> Result<SecretString, EbayError> {
        let slot = self.slot(credential);
        let mut s = slot.lock().await;
        if let Some(t) = fresh(&s) {
            return Ok(t);
        }
        match self.app.refresh(credential.expose()).await {
            Ok(grant) => {
                let token = grant.access_token.clone();
                *s = Some((grant.access_token.clone(), until(&grant)));
                Ok(token)
            }
            Err(e) => {
                if matches!(e, TokenError::Invalid(_)) {
                    drop(s);
                    self.forget(credential);
                }
                Err(refresh_failure(e, "access token"))
            }
        }
    }

    /// The application token, for public data.
    pub async fn application_token(&self) -> Result<SecretString, EbayError> {
        let mut s = self.app_token.lock().await;
        if let Some(t) = fresh(&s) {
            return Ok(t);
        }
        match self.app.application_token().await {
            Ok(grant) => {
                let token = grant.access_token.clone();
                *s = Some((grant.access_token.clone(), until(&grant)));
                Ok(token)
            }
            Err(TokenError::Invalid(why)) => {
                tracing::error!(why, "eBay refused this app an application token");
                Err(EbayError::Internal(format!(
                    "eBay refused this server an application token ({why}); the server's operator needs to check the eBay keyset"
                )))
            }
            Err(e) => Err(refresh_failure(e, "application token")),
        }
    }

    /// Forgets user access token `token` after eBay refused it, so the next call refreshes.
    pub async fn invalidate(&self, credential: &Credential, token: &SecretString) {
        let slot = self.slot(credential);
        let mut s = slot.lock().await;
        if s.as_ref()
            .is_some_and(|(t, _)| t.expose_secret() == token.expose_secret())
        {
            *s = None;
        }
    }

    /// Forgets the application token after eBay refused it.
    pub async fn invalidate_application(&self, token: &SecretString) {
        let mut s = self.app_token.lock().await;
        if s.as_ref()
            .is_some_and(|(t, _)| t.expose_secret() == token.expose_secret())
        {
            *s = None;
        }
    }

    fn forget(&self, credential: &Credential) {
        self.slots
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&credential.fingerprint());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> OAuthApp {
        mcp_server_core::install_crypto_provider();
        OAuthApp::new(
            reqwest::Client::new(),
            Url::parse("https://api.ebay.com").unwrap(),
            Url::parse("https://auth.ebay.com/oauth2/authorize").unwrap(),
            "AnnLee-MyApp-PRD-1".into(),
            "secret".to_string().into(),
            "Ann_Lee-AnnLee-MyApp-abc".into(),
        )
    }

    #[test]
    fn oauth_error_bodies_are_read() {
        assert_eq!(
            token_error_detail(r#"{"error":"invalid_grant","error_description":"expired"}"#)
                .as_deref(),
            Some("invalid_grant: expired")
        );
        assert_eq!(token_error_detail("<html>"), None);
    }

    #[test]
    fn the_sign_in_url_names_the_app_the_runame_and_the_scopes() {
        let app = app();
        let url = Url::parse(&app.authorize_url("st")).unwrap();
        assert_eq!(url.path(), "/oauth2/authorize");
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "AnnLee-MyApp-PRD-1");
        assert_eq!(q["redirect_uri"], "Ann_Lee-AnnLee-MyApp-abc");
        assert_eq!(q["response_type"], "code");
        assert!(q["scope"].contains("sell.inventory"));
        assert_eq!(q["state"], "st");
        assert_eq!(
            app.token_url().as_str(),
            "https://api.ebay.com/identity/v1/oauth2/token"
        );
        assert!(!format!("{app:?}").contains("secret"));
    }
}
