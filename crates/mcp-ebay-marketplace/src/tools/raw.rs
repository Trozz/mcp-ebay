//! Any other eBay REST endpoint, for what the other tools do not cover: a read, and a write.

use super::{EbayServer, choice, json_result, marketplace_arg, run};
use crate::api::{Auth, Call, Host};
use crate::error::{self, EbayError};
use axum::http::request::Parts;
use reqwest::Method;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// The APIs a raw path may reach, by first segment.
const ROOTS: &[&str] = &["sell", "buy", "commerce", "developer"];
const MAX_PATH_CHARS: usize = 300;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ApiGetParams {
    /// The path below https://api.ebay.com, e.g. sell/account/v1/rate_table or buy/browse/v1/item/v1|123|0.
    pub path: String,
    /// Query parameters, e.g. {"marketplace_id": "EBAY_GB"}.
    pub query: Option<Map<String, Value>>,
    /// Send this app's token instead of the signed-in person's, for public data (buy/browse, commerce/taxonomy). Default: false.
    pub as_app: Option<bool>,
    /// Sent as X-EBAY-C-MARKETPLACE-ID, e.g. EBAY_GB.
    pub marketplace: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ApiRequestParams {
    /// POST, PUT, PATCH or DELETE.
    pub method: String,
    /// The path below https://api.ebay.com, e.g. sell/account/v1/return_policy.
    pub path: String,
    pub query: Option<Map<String, Value>>,
    /// The JSON body.
    pub body: Option<Value>,
    /// Sent as X-EBAY-C-MARKETPLACE-ID, and as Content-Language its language (the Inventory API needs it), e.g. EBAY_GB.
    pub marketplace: Option<String>,
}

/// The host and path segments of a raw path given by a person or model: `sell/x`, `/sell/x` or a
/// full URL on this server's eBay API hosts.
fn raw_path(s: &EbayServer, raw: &str) -> error::Result<(Host, Vec<String>)> {
    let raw = raw.trim();
    let path = if let Ok(u) = url::Url::parse(raw) {
        let ok = u.host_str().is_some_and(|h| {
            h == "ebay.com" || h.ends_with(".ebay.com") || Some(h) == s.client.base().host_str()
        });
        if !ok {
            return Err(EbayError::invalid(
                "path must be on the eBay API, not another host",
            ));
        }
        if u.query().is_some() {
            return Err(EbayError::invalid(
                "put query parameters in query, not in path",
            ));
        }
        u.path().to_string()
    } else {
        raw.to_string()
    };
    if path.contains(['?', '#', '%', '\\']) {
        return Err(EbayError::invalid(
            "path must be a plain eBay API path; put query parameters in query",
        ));
    }
    let segments: Vec<String> = path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let ok = !segments.is_empty()
        && path.chars().count() <= MAX_PATH_CHARS
        && ROOTS.contains(&segments[0].as_str())
        && segments.iter().all(|s| {
            s != "."
                && s != ".."
                && s.chars().all(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '|' | ':' | ',')
                })
        });
    if !ok {
        return Err(EbayError::invalid(format!(
            "path must be an eBay REST API path starting with {}, such as sell/inventory/v1/offer",
            ROOTS.join("/, ")
        )));
    }
    let host = if segments.len() >= 2 && segments[0] == "commerce" && segments[1] == "identity" {
        Host::Identity
    } else {
        Host::Api
    };
    Ok((host, segments))
}

fn query_pairs(q: Option<&Map<String, Value>>) -> error::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for (k, v) in q.into_iter().flatten() {
        let v = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            _ => {
                return Err(EbayError::invalid(format!(
                    "query value {k} must be text, a number or true/false"
                )));
            }
        };
        out.push((k.clone(), v));
    }
    Ok(out)
}

fn answer(status: u16, body: Value, location: Option<String>) -> Value {
    let mut out = json!({"status": status});
    if !body.is_null() {
        out["body"] = body;
    }
    if let Some(l) = location {
        out["location"] = Value::from(l);
    }
    out
}

#[tool_router(router = raw_router, vis = "pub(crate)")]
impl EbayServer {
    #[tool(
        description = "GET any eBay REST endpoint below sell/, buy/, commerce/ or developer/ that the other tools do not cover, e.g. sell/account/v1/rate_table, sell/analytics/v1/seller_standards_profile or developer/analytics/v1_beta/rate_limit. Returns eBay's JSON as it is.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn api_get(
        &self,
        Parameters(p): Parameters<ApiGetParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("api_get", async {
            let (host, segments) = raw_path(self, &p.path)?;
            let mut call = Call::get("raw_get", &[]).on(host);
            call.segments = segments;
            call.query = query_pairs(p.query.as_ref())?;
            if p.marketplace.is_some() {
                call = call
                    .marketplace(marketplace_arg(p.marketplace.as_deref(), self.marketplace)?.id);
            }
            let a = if p.as_app.unwrap_or(false) {
                self.client.rest(Auth::App, &call).await?
            } else {
                self.caller(&parts)?.send(call).await?
            };
            Ok(json_result(&answer(a.status, a.body, a.location)))
        })
        .await
    }

    #[tool(
        description = "POST, PUT, PATCH or DELETE any eBay REST endpoint below sell/, buy/, commerce/ or developer/ as the signed-in account, for what the other tools do not cover, e.g. creating a business policy (POST sell/account/v1/return_policy). Show the person the request and get their agreement first.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub async fn api_request(
        &self,
        Parameters(p): Parameters<ApiRequestParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("api_request", async {
            let c = self.caller(&parts)?;
            let method = match choice(
                "method",
                Some(&p.method),
                &["POST", "PUT", "PATCH", "DELETE"],
            )? {
                Some("POST") => Method::POST,
                Some("PUT") => Method::PUT,
                Some("PATCH") => Method::PATCH,
                _ => Method::DELETE,
            };
            let (host, segments) = raw_path(self, &p.path)?;
            let mut call = Call::new(method, "raw_request", &[]).on(host);
            call.segments = segments;
            call.query = query_pairs(p.query.as_ref())?;
            if let Some(b) = p.body {
                call = call.body(b);
            }
            if p.marketplace.is_some() {
                let m = c.marketplace(p.marketplace.as_deref())?;
                call = call.marketplace(m.id).language(m.language);
            }
            let a = c.send(call).await?;
            Ok(json_result(&answer(a.status, a.body, a.location)))
        })
        .await
    }
}
