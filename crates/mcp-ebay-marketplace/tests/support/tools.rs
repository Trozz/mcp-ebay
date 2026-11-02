#![allow(dead_code)]
use super::fake_ebay::{CLIENT_ID, CLIENT_SECRET, FakeEbay, REFRESH, RUNAME};
use axum::http::request::Parts;
use mcp_ebay_marketplace::api::{ApiSettings, EbayClient, http_client};
use mcp_ebay_marketplace::config::marketplace;
use mcp_ebay_marketplace::oauth::{OAuthApp, Tokens};
use mcp_ebay_marketplace::tools::EbayServer;
use mcp_server_core::credential::Credential;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;

/// A client of the fake, with no delay between a failure and its retry.
pub fn client(fake: &FakeEbay) -> Arc<EbayClient> {
    let mut s = ApiSettings::new(fake.url.clone(), fake.url.clone());
    s.retry_delay = Duration::ZERO;
    let http = http_client(&s).unwrap();
    let mut auth = fake.url.clone();
    auth.set_path("/oauth2/authorize");
    let app = Arc::new(OAuthApp::new(
        http.clone(),
        fake.url.clone(),
        auth,
        CLIENT_ID.into(),
        CLIENT_SECRET.to_string().into(),
        RUNAME.into(),
    ));
    Arc::new(EbayClient::new(http, s, Tokens::new(app), false))
}

pub fn server(fake: &FakeEbay) -> EbayServer {
    EbayServer::new(client(fake), marketplace("EBAY_GB").unwrap())
}

/// The request details a tool sees for a person signed in with the fake's refresh token.
pub fn auth() -> Extension<Parts> {
    auth_with(REFRESH)
}

pub fn auth_with(refresh: &str) -> Extension<Parts> {
    let (mut parts, ()) = axum::http::Request::new(()).into_parts();
    parts.extensions.insert(Credential::new(refresh));
    Extension(parts)
}

pub fn no_auth() -> Extension<Parts> {
    let (parts, ()) = axum::http::Request::new(()).into_parts();
    Extension(parts)
}

pub fn params<T: DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
    Parameters(serde_json::from_value(v).expect("valid tool arguments"))
}

pub fn is_error(r: &CallToolResult) -> bool {
    r.is_error == Some(true)
}

pub fn text(r: &CallToolResult) -> String {
    r.content[0]
        .as_text()
        .expect("first content block is text")
        .text
        .clone()
}

pub fn json(r: &CallToolResult) -> serde_json::Value {
    assert!(!is_error(r), "tool error: {}", text(r));
    serde_json::from_str(&text(r)).expect("tool result is JSON")
}

pub fn error_text(r: &CallToolResult) -> String {
    assert!(is_error(r), "expected a tool error, got: {}", text(r));
    text(r)
}
