//! Thin client for eBay's APIs: the REST APIs (Sell, Buy, Commerce) with the caller's user access
//! token or the app's application token, and the XML Trading API for listings made on the eBay
//! website. No redirects, timeouts, one retry of reads after a transport error or a 5xx, one retry
//! with a fresh access token after a 401, status mapping and metrics. Writes are never retried
//! after they reach eBay, so a slow create cannot run twice.

use crate::error::{EbayError, Result};
use crate::oauth::Tokens;
use crate::xml;
use mcp_server_core::credential::Credential;
use mcp_server_core::observability::content_logging_enabled;
use reqwest::header::RETRY_AFTER;
use reqwest::{Method, StatusCode};
use secrecy::{ExposeSecret, SecretString};
use serde_json::Value;
use std::time::{Duration, Instant};
use url::Url;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const RETRY_DELAY: Duration = Duration::from_millis(250);
/// Largest response body read.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_ERROR_DETAIL: usize = 600;
/// The Trading API schema version this server speaks.
pub const TRADING_COMPATIBILITY_LEVEL: &str = "1349";
/// Trading API error codes meaning the token is not (or no longer) accepted.
const TRADING_AUTH_ERRORS: &[&str] = &["931", "932", "16110", "21916984", "21917053"];
/// Trading API error codes meaning a call limit is used up.
const TRADING_LIMIT_ERRORS: &[&str] = &["518", "21919144", "21919165"];

pub struct ApiSettings {
    /// `https://api.ebay.com`: REST APIs, the token endpoint and the Trading API.
    pub api: Url,
    /// `https://apiz.ebay.com`: the Identity API.
    pub identity: Url,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub retry_delay: Duration,
    /// Always log response bodies at debug level. Without it, bodies are logged only while
    /// `DEBUG_UNSAFE_LOG_CONTENT` is on.
    pub log_bodies: bool,
}

impl ApiSettings {
    pub fn new(api: Url, identity: Url) -> Self {
        ApiSettings {
            api,
            identity,
            connect_timeout: CONNECT_TIMEOUT,
            request_timeout: REQUEST_TIMEOUT,
            retry_delay: RETRY_DELAY,
            log_bodies: false,
        }
    }
}

/// The HTTP client every eBay request goes through: no redirects, timeouts, this server's user
/// agent.
pub fn http_client(settings: &ApiSettings) -> Result<reqwest::Client> {
    mcp_server_core::install_crypto_provider();
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(settings.connect_timeout)
        .timeout(settings.request_timeout)
        .user_agent(format!(
            "mcp-ebay/{} (+https://github.com/Trozz/mcp-ebay)",
            crate::VERSION
        ))
        .build()
        .map_err(|e| EbayError::Internal(format!("building the HTTP client failed: {e}")))
}

/// Whose token a request carries.
#[derive(Clone, Copy)]
pub enum Auth<'a> {
    /// The signed-in person's: selling, orders, their account.
    User(&'a Credential),
    /// This app's: public data such as search and categories.
    App,
}

/// Which eBay host a request goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    Api,
    Identity,
}

/// One REST request.
#[derive(Debug, Clone)]
pub struct Call {
    pub method: Method,
    /// A fixed label for metrics and logs.
    pub endpoint: &'static str,
    pub host: Host,
    /// Path segments below the host, each percent-encoded when sent.
    pub segments: Vec<String>,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
    pub headers: Vec<(&'static str, String)>,
}

impl Call {
    pub fn new(method: Method, endpoint: &'static str, segments: &[&str]) -> Self {
        Call {
            method,
            endpoint,
            host: Host::Api,
            segments: segments.iter().map(|s| s.to_string()).collect(),
            query: Vec::new(),
            body: None,
            headers: Vec::new(),
        }
    }

    pub fn get(endpoint: &'static str, segments: &[&str]) -> Self {
        Call::new(Method::GET, endpoint, segments)
    }

    pub fn query(mut self, name: &str, value: impl Into<String>) -> Self {
        self.query.push((name.to_string(), value.into()));
        self
    }

    /// Adds `name=value` when the value is present.
    pub fn query_opt(self, name: &str, value: Option<impl Into<String>>) -> Self {
        match value {
            Some(v) => self.query(name, v),
            None => self,
        }
    }

    pub fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }

    /// `X-EBAY-C-MARKETPLACE-ID`.
    pub fn marketplace(self, id: &str) -> Self {
        self.header("X-EBAY-C-MARKETPLACE-ID", id)
    }

    /// `Content-Language`, which the Inventory API needs on every write of listing content.
    pub fn language(self, lang: &str) -> Self {
        self.header("Content-Language", lang)
    }

    pub fn on(mut self, host: Host) -> Self {
        self.host = host;
        self
    }
}

/// An eBay answer.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: u16,
    pub body: Value,
    /// `Location` of a created resource.
    pub location: Option<String>,
}

pub struct EbayClient {
    http: reqwest::Client,
    settings: ApiSettings,
    tokens: Tokens,
    sandbox: bool,
}

fn status_class(status: Option<StatusCode>) -> &'static str {
    match status.map(|s| s.as_u16()) {
        Some(200..=299) => "2xx",
        Some(300..=399) => "3xx",
        Some(400..=499) => "4xx",
        Some(500..=599) => "5xx",
        _ => "error",
    }
}

fn clip(s: String) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > MAX_ERROR_DETAIL {
        let cut: String = s.chars().take(MAX_ERROR_DETAIL).collect();
        format!("{cut}...")
    } else {
        s
    }
}

/// A short, single-line message from an eBay REST error body:
/// `{"errors":[{"errorId":25002,"message":"...","longMessage":"...","parameters":[...]}]}`, an
/// OAuth-style `{"error":"...","error_description":"..."}`, or plain text.
pub fn error_detail(body: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    match serde_json::from_str::<Value>(body) {
        Ok(v) => {
            let errors = v
                .get("errors")
                .or_else(|| v.get("warnings"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for e in &errors {
                let message = e.get("message").and_then(Value::as_str).unwrap_or("");
                let long = e.get("longMessage").and_then(Value::as_str).unwrap_or("");
                let mut text = if !long.is_empty() && long != message {
                    format!("{message} {long}")
                } else {
                    message.to_string()
                };
                let params: Vec<String> = e
                    .get("parameters")
                    .and_then(Value::as_array)
                    .map(|ps| {
                        ps.iter()
                            .filter_map(|p| {
                                let n = p.get("name")?.as_str()?;
                                let val = p.get("value")?.as_str()?;
                                Some(format!("{n}={val}"))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if !params.is_empty() {
                    text.push_str(&format!(" ({})", params.join(", ")));
                }
                if let Some(id) = e.get("errorId").and_then(Value::as_i64) {
                    text.push_str(&format!(" [error {id}]"));
                }
                if !text.trim().is_empty() {
                    parts.push(text.trim().to_string());
                }
            }
            if errors.is_empty()
                && let Some(e) = v.get("error").and_then(Value::as_str)
            {
                let d = v
                    .get("error_description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                parts.push(format!("{e} {d}").trim().to_string());
            }
        }
        Err(_) => {
            let t = body.trim();
            if !t.is_empty() && !t.starts_with('<') {
                parts.push(t.to_string());
            }
        }
    }
    let joined = clip(parts.join("; "));
    (!joined.is_empty()).then_some(joined)
}

fn map_status(
    method: &Method,
    path: &str,
    status: StatusCode,
    body: &str,
    retry_after_secs: Option<u64>,
) -> EbayError {
    let detail = error_detail(body);
    let with_detail = |prefix: String| match &detail {
        Some(d) => format!("{prefix}: {d}"),
        None => prefix,
    };
    match status.as_u16() {
        401 => EbayError::Auth,
        403 => EbayError::Permission(with_detail(format!(
            "eBay refused {method} {path} (HTTP 403): the signed-in account may not be allowed this (for example, it is not set up to sell on this marketplace)"
        ))),
        404 => EbayError::NotFound(with_detail(format!("nothing found at {path}"))),
        429 => EbayError::RateLimited { retry_after_secs },
        400 | 405 | 409 | 412 | 415 | 422 => EbayError::Validation(with_detail(format!(
            "eBay rejected {method} {path} (HTTP {})",
            status.as_u16()
        ))),
        300..=399 => EbayError::Upstream(format!(
            "{method} {path} returned an unexpected redirect (HTTP {})",
            status.as_u16()
        )),
        code => EbayError::Upstream(with_detail(format!("{method} {path} returned HTTP {code}"))),
    }
}

impl EbayClient {
    pub fn new(
        http: reqwest::Client,
        settings: ApiSettings,
        tokens: Tokens,
        sandbox: bool,
    ) -> Self {
        EbayClient {
            http,
            settings,
            tokens,
            sandbox,
        }
    }

    pub fn base(&self) -> &Url {
        &self.settings.api
    }

    pub fn tokens(&self) -> &Tokens {
        &self.tokens
    }

    pub fn is_sandbox(&self) -> bool {
        self.sandbox
    }

    fn log_bodies(&self) -> bool {
        self.settings.log_bodies || content_logging_enabled()
    }

    /// The URL of `segments` (each percent-encoded) below `host`, with `query`.
    pub fn url(&self, host: Host, segments: &[String], query: &[(String, String)]) -> Url {
        let mut url = match host {
            Host::Api => self.settings.api.clone(),
            Host::Identity => self.settings.identity.clone(),
        };
        url.path_segments_mut()
            .expect("an http(s) base URL has path segments")
            .pop_if_empty()
            .extend(segments);
        // eBay's item ids hold `|`, which eBay wants percent-encoded; the url crate leaves it.
        if url.path().contains('|') {
            let path = url.path().replace('|', "%7C");
            url.set_path(&path);
        }
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        url
    }

    async fn token(&self, auth: Auth<'_>) -> Result<SecretString> {
        match auth {
            Auth::User(c) => self.tokens.access_token(c).await,
            Auth::App => self.tokens.application_token().await,
        }
    }

    async fn invalidate(&self, auth: Auth<'_>, token: &SecretString) {
        match auth {
            Auth::User(c) => self.tokens.invalidate(c, token).await,
            Auth::App => self.tokens.invalidate_application(token).await,
        }
    }

    /// Sends a REST request with `auth`'s token.
    pub async fn rest(&self, auth: Auth<'_>, call: &Call) -> Result<Answer> {
        let url = self.url(call.host, &call.segments, &call.query);
        let path = url.path().to_string();
        let method = &call.method;
        let endpoint = call.endpoint;
        let retryable = *method == Method::GET;
        let mut retried = false;
        let mut reauthorised = false;
        loop {
            let token = self.token(auth).await?;
            let started = Instant::now();
            let mut request = self
                .http
                .request(method.clone(), url.clone())
                .bearer_auth(token.expose_secret())
                .header(reqwest::header::ACCEPT, "application/json");
            for (k, v) in &call.headers {
                request = request.header(*k, v);
            }
            if let Some(body) = &call.body {
                request = request
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(body.to_string());
            } else if *method == Method::POST || *method == Method::PUT {
                // Some eBay actions (publish, withdraw) take no body but want a content type.
                request = request
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body("");
            }
            let result = request.send().await;
            let status = result.as_ref().ok().map(|r| r.status());
            metrics::counter!("ebay_requests_total", "endpoint" => endpoint, "status_class" => status_class(status))
                .increment(1);
            metrics::histogram!("ebay_request_duration_seconds", "endpoint" => endpoint)
                .record(started.elapsed().as_secs_f64());
            tracing::debug!(
                endpoint,
                method = %method,
                path = %path,
                status = status.map(|s| s.as_u16()),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "ebay request"
            );
            let response = match result {
                Ok(r) => r,
                Err(e) if retryable && !retried => {
                    tracing::debug!(endpoint, error = %describe(&e), "retrying");
                    retried = true;
                    tokio::time::sleep(self.settings.retry_delay).await;
                    continue;
                }
                Err(e) => {
                    let what = if e.is_timeout() {
                        "timed out"
                    } else {
                        "failed"
                    };
                    return Err(EbayError::Upstream(format!(
                        "{method} {path} {what}: {}",
                        describe(&e)
                    )));
                }
            };
            let status = response.status();
            // An access token can be revoked or expire early: refresh once and send again. The
            // request was refused before eBay acted on it, so this is safe for writes too.
            if status == StatusCode::UNAUTHORIZED && !reauthorised {
                reauthorised = true;
                self.invalidate(auth, &token).await;
                continue;
            }
            if status.is_server_error() && retryable && !retried {
                tracing::debug!(endpoint, status = status.as_u16(), "retrying");
                retried = true;
                tokio::time::sleep(self.settings.retry_delay).await;
                continue;
            }
            let headers = response.headers().clone();
            let retry_after_secs = headers
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.trim().parse::<u64>().ok());
            let text = read_body(response, &path).await?;
            if self.log_bodies() {
                tracing::debug!(endpoint, path = %path, status = status.as_u16(), body = %text, "ebay response");
            }
            if !status.is_success() {
                if status == StatusCode::TOO_MANY_REQUESTS {
                    metrics::counter!("ebay_rate_limited_total").increment(1);
                }
                return Err(map_status(method, &path, status, &text, retry_after_secs));
            }
            let body = if text.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&text).map_err(|e| {
                    EbayError::Upstream(format!(
                        "{method} {path} returned invalid JSON ({e}); check EBAY_API_URL points at the eBay API"
                    ))
                })?
            };
            return Ok(Answer {
                status: status.as_u16(),
                body,
                location: headers
                    .get(reqwest::header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string),
            });
        }
    }

    /// Calls Trading API `call` (such as `GetMyeBaySelling`) on site `site_id` as the signed-in
    /// person, with `inner` (already escaped XML) inside the request element. Returns the
    /// response element as JSON; warnings are left in its `Errors`.
    pub async fn trading(
        &self,
        credential: &Credential,
        call: &'static str,
        site_id: u32,
        inner: &str,
    ) -> Result<Value> {
        let mut url = self.settings.api.clone();
        url.path_segments_mut()
            .expect("an http(s) base URL has path segments")
            .pop_if_empty()
            .extend(["ws", "api.dll"]);
        let body = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><{call}Request xmlns=\"urn:ebay:apis:eBLBaseComponents\"><ErrorLanguage>en_GB</ErrorLanguage><WarningLevel>High</WarningLevel>{inner}</{call}Request>"
        );
        let retryable = call.starts_with("Get");
        let mut retried = false;
        let mut reauthorised = false;
        let auth = Auth::User(credential);
        loop {
            let token = self.token(auth).await?;
            let started = Instant::now();
            let result = self
                .http
                .post(url.clone())
                .header("X-EBAY-API-CALL-NAME", call)
                .header("X-EBAY-API-SITEID", site_id.to_string())
                .header(
                    "X-EBAY-API-COMPATIBILITY-LEVEL",
                    TRADING_COMPATIBILITY_LEVEL,
                )
                .header("X-EBAY-API-IAF-TOKEN", token.expose_secret())
                .header(reqwest::header::CONTENT_TYPE, "text/xml")
                .body(body.clone())
                .send()
                .await;
            let status = result.as_ref().ok().map(|r| r.status());
            metrics::counter!("ebay_requests_total", "endpoint" => call, "status_class" => status_class(status))
                .increment(1);
            metrics::histogram!("ebay_request_duration_seconds", "endpoint" => call)
                .record(started.elapsed().as_secs_f64());
            tracing::debug!(
                endpoint = call,
                status = status.map(|s| s.as_u16()),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "ebay trading request"
            );
            let response = match result {
                Ok(r) => r,
                Err(_) if retryable && !retried => {
                    retried = true;
                    tokio::time::sleep(self.settings.retry_delay).await;
                    continue;
                }
                Err(e) => {
                    return Err(EbayError::Upstream(format!(
                        "{call} failed: {}",
                        describe(&e)
                    )));
                }
            };
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED && !reauthorised {
                reauthorised = true;
                self.invalidate(auth, &token).await;
                continue;
            }
            if status.is_server_error() && retryable && !retried {
                retried = true;
                tokio::time::sleep(self.settings.retry_delay).await;
                continue;
            }
            let text = read_body(response, call).await?;
            if self.log_bodies() {
                tracing::debug!(endpoint = call, status = status.as_u16(), body = %text, "ebay trading response");
            }
            let value = match xml::parse(&text) {
                Ok(v) => v,
                Err(_) if !status.is_success() => {
                    return Err(map_status(&Method::POST, call, status, &text, None));
                }
                Err(e) => {
                    return Err(EbayError::Upstream(format!(
                        "{call} answered with XML that could not be read ({e})"
                    )));
                }
            };
            let ack = value.get("Ack").and_then(Value::as_str).unwrap_or("");
            if ack == "Success" || ack == "Warning" {
                return Ok(value);
            }
            let errors = trading_errors(&value);
            let codes: Vec<&str> = errors
                .iter()
                .filter(|e| e.severity != "Warning")
                .map(|e| e.code.as_str())
                .collect();
            if codes.iter().any(|c| TRADING_AUTH_ERRORS.contains(c)) {
                if !reauthorised {
                    reauthorised = true;
                    self.invalidate(auth, &token).await;
                    continue;
                }
                return Err(EbayError::Auth);
            }
            if codes.iter().any(|c| TRADING_LIMIT_ERRORS.contains(c)) {
                return Err(EbayError::RateLimited {
                    retry_after_secs: None,
                });
            }
            let detail = clip(
                errors
                    .iter()
                    .filter(|e| e.severity != "Warning")
                    .map(|e| e.text())
                    .collect::<Vec<_>>()
                    .join("; "),
            );
            return Err(if detail.is_empty() {
                EbayError::Upstream(format!("{call} failed (Ack {ack:?}) without saying why"))
            } else {
                EbayError::Validation(format!("eBay refused {call}: {detail}"))
            });
        }
    }
}

/// One entry of a Trading API answer's `Errors`.
pub struct TradingError {
    pub code: String,
    pub severity: String,
    pub short: String,
    pub long: String,
}

impl TradingError {
    pub fn text(&self) -> String {
        let msg = if !self.long.is_empty() && self.long != self.short {
            &self.long
        } else {
            &self.short
        };
        format!("{msg} [error {}]", self.code)
    }
}

/// The `Errors` of a Trading API answer, errors and warnings alike.
pub fn trading_errors(v: &Value) -> Vec<TradingError> {
    xml::list(v.get("Errors"))
        .into_iter()
        .map(|e| {
            let s = |k: &str| xml::text(e.get(k)).unwrap_or_default();
            TradingError {
                code: s("ErrorCode"),
                severity: s("SeverityCode"),
                short: s("ShortMessage"),
                long: s("LongMessage"),
            }
        })
        .collect()
}

/// The warnings of a Trading API answer, as text.
pub fn trading_warnings(v: &Value) -> Vec<String> {
    trading_errors(v)
        .into_iter()
        .filter(|e| e.severity == "Warning")
        .map(|e| e.text())
        .collect()
}

fn describe(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    // reqwest includes the URL, which carries query values; keep only the kind of failure.
    msg.split(" for url ").next().unwrap_or(&msg).to_string()
}

async fn read_body(mut response: reqwest::Response, path: &str) -> Result<String> {
    let too_big = || {
        EbayError::Upstream(format!(
            "{path} answered with more than {} MB",
            MAX_BODY_BYTES / (1024 * 1024)
        ))
    };
    if response
        .content_length()
        .is_some_and(|n| n as usize > MAX_BODY_BYTES)
    {
        return Err(too_big());
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| EbayError::Upstream(format!("reading {path} failed: {}", describe(&e))))?
    {
        if buf.len() + chunk.len() > MAX_BODY_BYTES {
            return Err(too_big());
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rest_error_bodies_are_read() {
        assert_eq!(
            error_detail(
                r#"{"errors":[{"errorId":25002,"domain":"API_INVENTORY","message":"A user error has occurred.","longMessage":"Offer entity already exists.","parameters":[{"name":"offerId","value":"123"}]}]}"#
            )
            .as_deref(),
            Some("A user error has occurred. Offer entity already exists. (offerId=123) [error 25002]")
        );
        assert_eq!(
            error_detail(r#"{"error":"invalid_scope","error_description":"bad"}"#).as_deref(),
            Some("invalid_scope bad")
        );
        assert_eq!(error_detail("<html>oops</html>"), None);
        assert_eq!(error_detail(""), None);
    }

    #[test]
    fn statuses_map_to_kinds() {
        let m = Method::GET;
        for (status, kind) in [
            (StatusCode::UNAUTHORIZED, "auth"),
            (StatusCode::FORBIDDEN, "permission"),
            (StatusCode::NOT_FOUND, "not_found"),
            (StatusCode::BAD_REQUEST, "validation"),
            (StatusCode::CONFLICT, "validation"),
            (StatusCode::BAD_GATEWAY, "upstream"),
        ] {
            assert_eq!(
                map_status(&m, "/x", status, "", None).kind(),
                kind,
                "{status}"
            );
        }
        assert!(matches!(
            map_status(&m, "/x", StatusCode::TOO_MANY_REQUESTS, "", Some(60)),
            EbayError::RateLimited {
                retry_after_secs: Some(60)
            }
        ));
    }

    #[test]
    fn calls_are_built_with_encoded_segments() {
        let call = Call::get("item", &["buy", "browse", "v1", "item", "v1|123|0"])
            .query("fieldgroups", "PRODUCT")
            .marketplace("EBAY_GB");
        assert_eq!(call.headers[0].0, "X-EBAY-C-MARKETPLACE-ID");
        let settings = ApiSettings::new(
            Url::parse("https://api.ebay.com").unwrap(),
            Url::parse("https://apiz.ebay.com").unwrap(),
        );
        mcp_server_core::install_crypto_provider();
        let app = std::sync::Arc::new(crate::oauth::OAuthApp::new(
            reqwest::Client::new(),
            settings.api.clone(),
            Url::parse("https://auth.ebay.com/oauth2/authorize").unwrap(),
            "id".into(),
            "s".to_string().into(),
            "r".into(),
        ));
        let c = EbayClient::new(reqwest::Client::new(), settings, Tokens::new(app), false);
        assert_eq!(
            c.url(call.host, &call.segments, &call.query).as_str(),
            "https://api.ebay.com/buy/browse/v1/item/v1%7C123%7C0?fieldgroups=PRODUCT"
        );
        assert_eq!(
            c.url(Host::Identity, &["commerce".into(), "identity".into()], &[])
                .as_str(),
            "https://apiz.ebay.com/commerce/identity"
        );
        assert_eq!(
            c.url(Host::Api, &["sell".into(), "a/b".into()], &[])
                .as_str(),
            "https://api.ebay.com/sell/a%2Fb"
        );
    }
}
