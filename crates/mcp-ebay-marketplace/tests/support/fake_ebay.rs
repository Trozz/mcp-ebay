//! An in-memory eBay shaped like the real one: the OAuth token endpoint with HTTP Basic client
//! authentication (authorization codes for the RuName, refresh tokens, client credentials),
//! user and application tokens kept apart, the Trading API with `X-EBAY-API-IAF-TOKEN`, canned
//! answers that tests set per method and path, and a log of every request for assertions.
#![allow(dead_code)]

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use url::Url;

pub const CLIENT_ID: &str = "AnnLee-MyApp-PRD-1a2b3c4d5-6e7f8a9b";
pub const CLIENT_SECRET: &str = "PRD-1a2b3c4d5e6f-7a8b";
pub const RUNAME: &str = "Ann_Lee-AnnLee-MyApp-abcdefgh";
/// The authorization code the fake grants for, shaped like eBay's.
pub const CODE: &str = "v^1.1#i^1#good-code";
/// The refresh token the code grants.
pub const REFRESH: &str = "v^1.1#i^1#refresh-0";

#[derive(Debug, Clone)]
pub struct Logged {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
    pub raw_body: String,
    pub form: Vec<(String, String)>,
}

impl Logged {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn form(&self, name: &str) -> Option<&str> {
        self.form
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// A canned answer.
#[derive(Debug, Clone)]
pub struct Canned {
    pub status: u16,
    pub body: String,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
}

impl Canned {
    pub fn ok(body: Value) -> Self {
        Canned::status(200, body)
    }
    pub fn status(status: u16, body: Value) -> Self {
        Canned {
            status,
            body: if body.is_null() {
                String::new()
            } else {
                body.to_string()
            },
            content_type: "application/json",
            headers: Vec::new(),
        }
    }
    pub fn xml(body: &str) -> Self {
        Canned {
            status: 200,
            body: body.to_string(),
            content_type: "text/xml",
            headers: Vec::new(),
        }
    }
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

#[derive(Default)]
pub struct FakeState {
    pub log: Vec<Logged>,
    /// Answers by "METHOD /path" (Trading calls by "TRADING CallName"); a list is served in
    /// order, the last one repeatedly.
    pub canned: HashMap<String, Vec<Canned>>,
    /// Valid user access tokens.
    pub user_tokens: Vec<String>,
    /// Valid application tokens.
    pub app_tokens: Vec<String>,
    /// Valid refresh tokens.
    pub refresh: Vec<String>,
    pub refreshes: usize,
    pub app_grants: usize,
    pub codes_used: usize,
    pub expires_in: u64,
}

#[derive(Clone)]
pub struct FakeEbay {
    pub url: Url,
    pub state: Arc<Mutex<FakeState>>,
}

impl FakeEbay {
    pub async fn start() -> Self {
        let state = Arc::new(Mutex::new(FakeState {
            refresh: vec![REFRESH.to_string()],
            expires_in: 7200,
            ..Default::default()
        }));
        let app = axum::Router::new()
            .fallback(handle)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let fake = FakeEbay {
            url: Url::parse(&format!("http://{addr}")).unwrap(),
            state,
        };
        fake.defaults();
        fake
    }

    pub fn lock(&self) -> MutexGuard<'_, FakeState> {
        self.state.lock().unwrap()
    }

    /// Serves `answer` for `method` `path` (from the root, without a leading slash).
    pub fn on(&self, method: &str, path: &str, answer: Canned) {
        self.lock()
            .canned
            .insert(format!("{method} /{path}"), vec![answer]);
    }

    /// Serves `answers` in order for `method` `path`.
    pub fn on_seq(&self, method: &str, path: &str, answers: Vec<Canned>) {
        self.lock()
            .canned
            .insert(format!("{method} /{path}"), answers);
    }

    /// Serves `xml` for Trading API call `call`.
    pub fn on_trading(&self, call: &str, xml: &str) {
        self.lock()
            .canned
            .insert(format!("TRADING {call}"), vec![Canned::xml(xml)]);
    }

    /// Requests other than to the token endpoint.
    pub fn api_log(&self) -> Vec<Logged> {
        self.lock()
            .log
            .iter()
            .filter(|l| !l.path.ends_with("/oauth2/token"))
            .cloned()
            .collect()
    }

    /// The requests to `method` `path`.
    pub fn calls(&self, method: &str, path: &str) -> Vec<Logged> {
        let full = format!("/{path}");
        self.lock()
            .log
            .iter()
            .filter(|l| l.method == method && l.path == full)
            .cloned()
            .collect()
    }

    /// The last request to `method` `path`.
    pub fn last(&self, method: &str, path: &str) -> Logged {
        self.calls(method, path)
            .pop()
            .unwrap_or_else(|| panic!("no {method} /{path} was made"))
    }

    /// The last Trading API call `call`.
    pub fn last_trading(&self, call: &str) -> Logged {
        self.lock()
            .log
            .iter()
            .rev()
            .find(|l| l.path == "/ws/api.dll" && l.header("x-ebay-api-call-name") == Some(call))
            .cloned()
            .unwrap_or_else(|| panic!("no Trading {call} was made"))
    }

    fn defaults(&self) {
        self.on(
            "GET",
            "commerce/identity/v1/user/",
            Canned::ok(json!({"userId": "u-1", "username": "ann_sells", "accountType": "INDIVIDUAL", "registrationMarketplaceId": "EBAY_GB"})),
        );
        self.on(
            "GET",
            "commerce/taxonomy/v1/get_default_category_tree_id",
            Canned::ok(json!({"categoryTreeId": "3", "categoryTreeVersion": "1"})),
        );
        // A seller ready to list: one of each policy and one location.
        self.on(
            "GET",
            "sell/account/v1/fulfillment_policy",
            Canned::ok(json!({"total": 1, "fulfillmentPolicies": [{"fulfillmentPolicyId": "F1", "name": "Royal Mail 2nd", "marketplaceId": "EBAY_GB", "categoryTypes": [{"name": "ALL_EXCLUDING_MOTORS_VEHICLES", "default": true}]}]})),
        );
        self.on(
            "GET",
            "sell/account/v1/payment_policy",
            Canned::ok(json!({"total": 1, "paymentPolicies": [{"paymentPolicyId": "P1", "name": "eBay payments", "marketplaceId": "EBAY_GB", "categoryTypes": [{"name": "ALL_EXCLUDING_MOTORS_VEHICLES", "default": true}]}]})),
        );
        self.on(
            "GET",
            "sell/account/v1/return_policy",
            Canned::ok(json!({"total": 1, "returnPolicies": [{"returnPolicyId": "R1", "name": "30 days", "marketplaceId": "EBAY_GB", "returnsAccepted": true, "returnPeriod": {"value": 30, "unit": "DAY"}, "categoryTypes": [{"name": "ALL_EXCLUDING_MOTORS_VEHICLES", "default": true}]}]})),
        );
        self.on(
            "GET",
            "sell/inventory/v1/location",
            Canned::ok(json!({"total": 1, "locations": [{"merchantLocationKey": "home", "name": "Home", "merchantLocationStatus": "ENABLED", "location": {"address": {"city": "York", "postalCode": "YO1 1AA", "country": "GB"}}}]})),
        );
        self.on(
            "GET",
            "sell/account/v1/program/get_opted_in_programs",
            Canned::ok(json!({"programs": [{"programType": "SELLING_POLICY_MANAGEMENT"}]})),
        );
    }
}

fn basic_ok(headers: &HeaderMap) -> bool {
    let want = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{CLIENT_ID}:{CLIENT_SECRET}"))
    );
    headers.get("authorization").and_then(|v| v.to_str().ok()) == Some(want.as_str())
}

fn respond(
    status: u16,
    content_type: &str,
    body: String,
    headers: &[(&'static str, String)],
) -> Response {
    let mut res = (
        StatusCode::from_u16(status).unwrap(),
        [("content-type", content_type.to_string())],
        body,
    )
        .into_response();
    for (k, v) in headers {
        res.headers_mut().insert(*k, v.parse().unwrap());
    }
    res
}

fn json_response(status: u16, body: &Value) -> Response {
    respond(
        status,
        "application/json",
        if body.is_null() {
            String::new()
        } else {
            body.to_string()
        },
        &[],
    )
}

fn serve(a: Canned) -> Response {
    respond(a.status, a.content_type, a.body, &a.headers)
}

async fn handle(
    State(state): State<Arc<Mutex<FakeState>>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let query: Vec<(String, String)> =
        url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
            .into_owned()
            .collect();
    let form: Vec<(String, String)> = if headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|c| c.starts_with("application/x-www-form-urlencoded"))
    {
        url::form_urlencoded::parse(&body).into_owned().collect()
    } else {
        Vec::new()
    };
    let json_body: Option<Value> = serde_json::from_slice(&body).ok();
    let mut st = state.lock().unwrap();
    st.log.push(Logged {
        method: method.to_string(),
        path: path.clone(),
        query: query.clone(),
        headers: headers
            .iter()
            .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
            .collect(),
        body: json_body.clone(),
        raw_body: String::from_utf8_lossy(&body).into_owned(),
        form: form.clone(),
    });

    if path == "/identity/v1/oauth2/token" {
        if !basic_ok(&headers) {
            return json_response(
                401,
                &json!({"error": "invalid_client", "error_description": "client authentication failed"}),
            );
        }
        let f = |k: &str| form.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let expires_in = st.expires_in;
        match f("grant_type").as_deref() {
            Some("authorization_code") => {
                if f("code").as_deref() != Some(CODE)
                    || f("redirect_uri").as_deref() != Some(RUNAME)
                    || st.codes_used > 0
                {
                    return json_response(
                        400,
                        &json!({"error": "invalid_grant", "error_description": "the provided authorization grant code is invalid or was issued to another client"}),
                    );
                }
                st.codes_used += 1;
                st.user_tokens.push("user-0".into());
                return json_response(
                    200,
                    &json!({"access_token": "user-0", "expires_in": expires_in, "refresh_token": REFRESH, "refresh_token_expires_in": 47304000, "token_type": "User Access Token"}),
                );
            }
            Some("refresh_token") => {
                let given = f("refresh_token").unwrap_or_default();
                if !st.refresh.contains(&given) || f("scope").is_none() {
                    return json_response(
                        400,
                        &json!({"error": "invalid_grant", "error_description": "the provided authorization refresh token is invalid or was issued to another client"}),
                    );
                }
                st.refreshes += 1;
                let access = format!("user-{}", st.refreshes);
                st.user_tokens.push(access.clone());
                return json_response(
                    200,
                    &json!({"access_token": access, "expires_in": expires_in, "token_type": "User Access Token"}),
                );
            }
            Some("client_credentials") => {
                st.app_grants += 1;
                let access = format!("app-{}", st.app_grants);
                st.app_tokens.push(access.clone());
                return json_response(
                    200,
                    &json!({"access_token": access, "expires_in": expires_in, "token_type": "Application Access Token"}),
                );
            }
            _ => return json_response(400, &json!({"error": "unsupported_grant_type"})),
        }
    }

    if path == "/ws/api.dll" {
        let token = headers
            .get("x-ebay-api-iaf-token")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let call = headers
            .get("x-ebay-api-call-name")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        if !st.user_tokens.contains(&token) {
            return respond(
                200,
                "text/xml",
                format!(
                    "<?xml version=\"1.0\"?><{call}Response xmlns=\"urn:ebay:apis:eBLBaseComponents\"><Ack>Failure</Ack><Errors><ShortMessage>Invalid IAF token.</ShortMessage><LongMessage>IAF token supplied is invalid.</LongMessage><ErrorCode>21916984</ErrorCode><SeverityCode>Error</SeverityCode></Errors></{call}Response>"
                ),
                &[],
            );
        }
        if let Some(answers) = st.canned.get_mut(&format!("TRADING {call}")) {
            let a = if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers[0].clone()
            };
            return serve(a);
        }
        return respond(
            200,
            "text/xml",
            format!(
                "<?xml version=\"1.0\"?><{call}Response xmlns=\"urn:ebay:apis:eBLBaseComponents\"><Ack>Success</Ack></{call}Response>"
            ),
            &[],
        );
    }

    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    // Public APIs take either token; selling and identity need a user's.
    let public = path.starts_with("/buy/browse")
        || path.starts_with("/commerce/taxonomy")
        || path.starts_with("/sell/metadata");
    let ok = st.user_tokens.contains(&token) || (public && st.app_tokens.contains(&token));
    if !ok {
        return json_response(
            401,
            &json!({"errors": [{"errorId": 1001, "domain": "OAuth", "category": "REQUEST", "message": "Invalid access token", "longMessage": "Invalid access token. Check the value of the Authorization HTTP request header."}]}),
        );
    }
    let key = format!("{method} {path}");
    if let Some(answers) = st.canned.get_mut(&key) {
        let a = if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers[0].clone()
        };
        return serve(a);
    }
    if method == Method::PUT || method == Method::DELETE {
        return json_response(204, &Value::Null);
    }
    if method == Method::POST {
        return json_response(200, &json!({}));
    }
    json_response(
        404,
        &json!({"errors": [{"errorId": 25710, "domain": "API_INVENTORY", "message": "We didn't find the resource/entity you are requesting."}]}),
    )
}
