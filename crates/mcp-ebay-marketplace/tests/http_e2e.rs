//! The whole server end to end, built as the binary builds it: a client registers, the person
//! signs in through (fake) eBay's OAuth, and the MCP endpoints then act with their eBay sign-in.
mod support;

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use http_body_util::BodyExt;
use mcp_server_core::app::{Server, SignIn};
use mcp_server_core::service::{HttpSettings, ServiceContext, ServiceEnv};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;
use support::fake_ebay::{CLIENT_ID, CLIENT_SECRET, CODE, Canned, FakeEbay, RUNAME};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

const PUBLIC: &str = "https://ebay-mcp.example.com";
const HOST: &str = "ebay-mcp.example.com";
const CLIENT_REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mJ92K9fwhzDHvM5HDkyX5Dw1mEtK8k";

async fn app(fake: &FakeEbay) -> axum::Router {
    app_with(fake, &[]).await
}

async fn app_with(fake: &FakeEbay, extra: &[(&str, &str)]) -> axum::Router {
    let api = fake.url.to_string();
    let auth = format!("{}oauth2/authorize", fake.url);
    let vars: HashMap<String, String> = [
        ("PUBLIC_URL", PUBLIC),
        ("OAUTH_SIGNING_KEY", "0123456789abcdef0123456789abcdef"),
        ("INSECURE_HTTP", "true"),
        ("EBAY_CLIENT_ID", CLIENT_ID),
        ("EBAY_CLIENT_SECRET", CLIENT_SECRET),
        ("EBAY_RUNAME", RUNAME),
        ("EBAY_API_URL", api.as_str()),
        ("EBAY_AUTH_URL", auth.as_str()),
    ]
    .into_iter()
    .chain(extra.iter().copied())
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let mut env = ServiceEnv::new(Box::new(move |k: &str| vars.get(k).cloned()));
    let server = Server::new("mcp-ebay", "test")
        .env_prefix("EBAY")
        .service("ebay", mcp_ebay_marketplace::from_env)
        .sign_in(SignIn::ServiceCredential);
    let loaded = server
        .load(&mut env)
        .unwrap_or_else(|| panic!("config errors: {:?}", env.errors));
    let ctx = ServiceContext {
        http: HttpSettings::from_core(&loaded.core),
        shutdown: CancellationToken::new(),
    };
    server.router(&loaded, &ctx)
}

async fn send(app: &axum::Router, req: Request<Body>) -> Response<Body> {
    app.clone().oneshot(req).await.unwrap()
}

async fn body_text(res: Response<Body>) -> String {
    String::from_utf8(res.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
}

fn param(url: &url::Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

fn form(uri: &str, pairs: &[(&str, &str)]) -> Request<Body> {
    let body = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    Request::post(uri)
        .header("host", HOST)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .unwrap()
}

/// Signs in through eBay for `resource` and returns an access token.
async fn sign_in(app: &axum::Router, fake: &FakeEbay, resource: &str) -> String {
    let res = send(
        app,
        Request::post("/register")
            .header("host", HOST)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"redirect_uris": [CLIENT_REDIRECT], "client_name": "Claude"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    let reg: Value = serde_json::from_str(&body_text(res).await).unwrap();
    let cid = reg["client_id"].as_str().unwrap().to_string();
    let challenge = mcp_server_core::pkce::s256(VERIFIER);
    let pairs = [
        ("response_type", "code"),
        ("client_id", cid.as_str()),
        ("redirect_uri", CLIENT_REDIRECT),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", "st"),
        ("resource", resource),
    ];
    let q = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs(pairs)
        .finish();
    let html = body_text(
        send(
            app,
            Request::get(format!("/authorize?{q}"))
                .header("host", HOST)
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert!(html.contains("Continue to eBay"), "{html}");
    let at = html.find("name=\"csrf\" value=\"").unwrap() + "name=\"csrf\" value=\"".len();
    let csrf = html[at..at + html[at..].find('"').unwrap()].to_string();
    let mut f: Vec<(&str, &str)> = pairs.to_vec();
    f.push(("scope", resource.rsplit('/').next().unwrap()));
    f.push(("csrf", &csrf));
    let res = send(app, form("/authorize", &f)).await;
    assert_eq!(res.status(), StatusCode::SEE_OTHER);
    let to = url::Url::parse(res.headers()["location"].to_str().unwrap()).unwrap();
    let cookie = res.headers()["set-cookie"]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    // The person is sent to eBay's sign-in page for this server's app, with the RuName as the
    // redirect URI and the selling scopes.
    assert_eq!(to.path(), "/oauth2/authorize");
    assert_eq!(param(&to, "client_id").as_deref(), Some(CLIENT_ID));
    assert_eq!(param(&to, "redirect_uri").as_deref(), Some(RUNAME));
    assert!(param(&to, "scope").unwrap().contains("sell.fulfillment"));
    let state = param(&to, "state").unwrap();

    // eBay sends them back to the RuName's accepted URL, PUBLIC_URL/callback, with a code.
    let cb = url::form_urlencoded::Serializer::new(String::new())
        .extend_pairs([
            ("state", state.as_str()),
            ("code", CODE),
            ("expires_in", "299"),
        ])
        .finish();
    let res = send(
        app,
        Request::get(format!("/callback?{cb}"))
            .header("host", HOST)
            .header("cookie", cookie)
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        res.status(),
        StatusCode::SEE_OTHER,
        "{}",
        body_text(res).await
    );
    let back = url::Url::parse(res.headers()["location"].to_str().unwrap()).unwrap();
    assert!(back.as_str().starts_with(CLIENT_REDIRECT));
    let code = param(&back, "code").unwrap();
    // The exchange named the RuName too.
    let ex = fake.last("POST", "identity/v1/oauth2/token");
    assert_eq!(ex.form("redirect_uri"), Some(RUNAME));
    assert_eq!(ex.form("code"), Some(CODE));

    let res = send(
        app,
        form(
            "/token",
            &[
                ("grant_type", "authorization_code"),
                ("code", &code),
                ("client_id", &cid),
                ("code_verifier", VERIFIER),
                ("redirect_uri", CLIENT_REDIRECT),
            ],
        ),
    )
    .await;
    let tok: Value = serde_json::from_str(&body_text(res).await).unwrap();
    tok["access_token"]
        .as_str()
        .expect("an access token")
        .to_string()
}

fn rpc(path: &str, token: &str, body: Value, session: Option<&str>) -> Request<Body> {
    let mut b = Request::post(path)
        .header("host", HOST)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream");
    if let Some(s) = session {
        b = b
            .header("mcp-session-id", s)
            .header("mcp-protocol-version", "2025-06-18");
    }
    b.body(Body::from(body.to_string())).unwrap()
}

/// The first JSON-RPC message of an SSE (or JSON) response.
async fn message(res: Response<Body>) -> Value {
    let mut body = res.into_body();
    let mut buf = String::new();
    loop {
        if let Ok(v) = serde_json::from_str::<Value>(buf.trim()) {
            return v;
        }
        for line in buf.lines() {
            if let Some(data) = line.strip_prefix("data:")
                && let Ok(v) = serde_json::from_str::<Value>(data.trim())
                && v.get("jsonrpc").is_some()
            {
                return v;
            }
        }
        let frame = tokio::time::timeout(Duration::from_secs(5), body.frame())
            .await
            .expect("a response within 5 s")
            .expect("a message before the body ends")
            .unwrap();
        if let Ok(d) = frame.into_data() {
            buf.push_str(&String::from_utf8_lossy(&d));
        }
    }
}

async fn session(app: &axum::Router, path: &str, token: &str) -> String {
    let res = send(
        app,
        rpc(
            path,
            token,
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e", "version": "1"}}}),
            None,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let id = res.headers()["mcp-session-id"]
        .to_str()
        .unwrap()
        .to_string();
    let init = message(res).await;
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("eBay")
    );
    let res = send(
        app,
        rpc(
            path,
            token,
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            Some(&id),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::ACCEPTED);
    id
}

async fn call(
    app: &axum::Router,
    path: &str,
    token: &str,
    sid: &str,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    message(
        send(
            app,
            rpc(
                path,
                token,
                json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}),
                Some(sid),
            ),
        )
        .await,
    )
    .await
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn people_sign_in_through_ebay_and_tools_act_as_them() {
    let fake = FakeEbay::start().await;
    let app = app(&fake).await;
    let token = sign_in(&app, &fake, "https://ebay-mcp.example.com/ebay").await;
    let sid = session(&app, "/ebay", &token).await;

    let list = call(&app, "/ebay", &token, &sid, 2, "tools/list", json!({})).await;
    let names = tool_names(&list);
    for want in [
        "create_listing",
        "update_listing",
        "delete_listing",
        "search_items",
        "list_orders",
    ] {
        assert!(names.iter().any(|n| n == want), "{want}: {names:?}");
    }

    let r = call(
        &app,
        "/ebay",
        &token,
        &sid,
        3,
        "tools/call",
        json!({"name": "get_seller_setup", "arguments": {}}),
    )
    .await;
    let text = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("ann_sells"), "{r}");
    // The tool used the access token granted at sign-in: no refresh was needed.
    assert_eq!(fake.lock().refreshes, 0);
    assert_eq!(
        fake.last("GET", "sell/account/v1/return_policy")
            .header("authorization"),
        Some("Bearer user-0")
    );

    // Search works for the signed-in person with the app's own token.
    fake.on(
        "GET",
        "buy/browse/v1/item_summary/search",
        Canned::ok(json!({"total": 0, "itemSummaries": []})),
    );
    let r = call(
        &app,
        "/ebay",
        &token,
        &sid,
        4,
        "tools/call",
        json!({"name": "search_items", "arguments": {"query": "lamp"}}),
    )
    .await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert!(
        fake.last("GET", "buy/browse/v1/item_summary/search")
            .header("authorization")
            .unwrap()
            .starts_with("Bearer app-")
    );

    let p = call(
        &app,
        "/ebay",
        &token,
        &sid,
        5,
        "prompts/get",
        json!({"name": "list_item", "arguments": {"item": "an Anglepoise lamp"}}),
    )
    .await;
    let t = p["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(
        t.contains("<item>an Anglepoise lamp</item>") && t.contains("`create_listing`"),
        "{t}"
    );
}

#[tokio::test]
async fn the_combined_endpoint_prefixes_tools_and_prompts() {
    let fake = FakeEbay::start().await;
    let app = app(&fake).await;
    let token = sign_in(&app, &fake, "https://ebay-mcp.example.com/mcp").await;
    let sid = session(&app, "/mcp", &token).await;
    let list = call(&app, "/mcp", &token, &sid, 2, "tools/list", json!({})).await;
    assert!(tool_names(&list).iter().any(|n| n == "ebay_create_listing"));
    let p = call(
        &app,
        "/mcp",
        &token,
        &sid,
        3,
        "prompts/get",
        json!({"name": "ebay_ship_orders", "arguments": {}}),
    )
    .await;
    let t = p["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(t.contains("`ebay_mark_order_shipped`"), "{t}");
}

#[tokio::test]
async fn read_only_mode_hides_every_write_tool() {
    let fake = FakeEbay::start().await;
    let app = app_with(&fake, &[("EBAY_READ_ONLY", "ebay")]).await;
    let token = sign_in(&app, &fake, "https://ebay-mcp.example.com/ebay").await;
    let sid = session(&app, "/ebay", &token).await;
    let names = tool_names(&call(&app, "/ebay", &token, &sid, 2, "tools/list", json!({})).await);
    for hidden in [
        "create_listing",
        "update_listing",
        "delete_listing",
        "end_listing",
        "mark_order_shipped",
        "api_request",
    ] {
        assert!(
            !names.iter().any(|n| n == hidden),
            "{hidden} is listed: {names:?}"
        );
    }
    for kept in ["search_items", "get_listing", "list_orders", "api_get"] {
        assert!(names.iter().any(|n| n == kept), "{kept}: {names:?}");
    }
}

#[tokio::test]
async fn without_a_sign_in_the_endpoints_point_at_the_oauth_metadata() {
    let fake = FakeEbay::start().await;
    let app = app(&fake).await;
    let res = send(&app, rpc("/ebay", "not-a-token", json!({}), None)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let h = res.headers()["www-authenticate"].to_str().unwrap();
    assert!(
        h.contains("/.well-known/oauth-protected-resource/ebay"),
        "{h}"
    );
    // An eBay token is not accepted in place of signing in.
    let res = send(&app, rpc("/ebay", "user-0", json!({}), None)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}
