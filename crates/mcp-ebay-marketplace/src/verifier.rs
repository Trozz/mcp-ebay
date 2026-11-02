//! Signing people in through eBay's own OAuth: the sign-in page sends them to eBay to grant this
//! app access, and their eBay refresh token is sealed into their MCP tokens.

use crate::api::{Auth, Call, EbayClient, Host};
use crate::oauth::TokenError;
use mcp_server_core::credential::{Credential, CredentialVerifier};
use mcp_server_core::upstream::{
    Exchange, ExchangeFuture, UpstreamGrant, UpstreamSignIn, UpstreamVerifier,
};
use serde_json::Value;
use std::sync::Arc;

/// The label people see on the sign-in page. Part of the sign-in version: changing it signs
/// everyone out.
pub const LABEL: &str = "eBay account";

pub struct EbaySignIn {
    client: Arc<EbayClient>,
}

impl EbaySignIn {
    pub fn new(client: Arc<EbayClient>) -> Self {
        EbaySignIn { client }
    }

    /// Who signed in, from the Identity API: `ebay:<username>`, stable across sign-ins. `None`
    /// when eBay does not say.
    async fn subject(&self, credential: &Credential) -> Option<String> {
        let a = self
            .client
            .rest(
                Auth::User(credential),
                &Call::get("identity_user", &["commerce", "identity", "v1", "user", ""])
                    .on(Host::Identity),
            )
            .await
            .ok()?;
        let name = a
            .body
            .get("username")
            .or_else(|| a.body.get("userId"))
            .and_then(Value::as_str)?;
        let ok = !name.is_empty()
            && name.len() <= 64
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '*'));
        ok.then(|| format!("ebay:{name}"))
    }
}

impl UpstreamSignIn for EbaySignIn {
    fn provider(&self) -> &str {
        "eBay"
    }

    /// eBay takes the RuName, not this server's `/callback` URL, as `redirect_uri`: the RuName's
    /// auth accepted URL in the eBay developer portal must be `PUBLIC_URL/callback`.
    fn authorize_url(&self, _redirect_uri: &str, state: &str) -> String {
        self.client.tokens().app().authorize_url(state)
    }

    fn exchange<'a>(&'a self, code: &'a str, _redirect_uri: &'a str) -> ExchangeFuture<'a> {
        Box::pin(async move {
            let grant = match self.client.tokens().app().exchange(code).await {
                Ok(g) => g,
                Err(e) => return verdict(e),
            };
            let Some(refresh) = grant.refresh_token.as_ref() else {
                return Exchange::Unavailable("eBay granted no refresh token".into());
            };
            let refresh = secrecy::ExposeSecret::expose_secret(refresh).to_string();
            self.client.tokens().seed(&refresh, &grant);
            let credential = Credential::new(refresh.clone());
            let subject = self.subject(&credential).await;
            tracing::info!(
                subject = subject.as_deref().unwrap_or("unknown"),
                "signed in to eBay"
            );
            let out = UpstreamGrant::new(refresh);
            Exchange::Granted(match subject {
                Some(s) => out.with_subject(s),
                None => out,
            })
        })
    }
}

/// How a failed code exchange is reported on the sign-in page.
fn verdict(e: TokenError) -> Exchange {
    match e {
        TokenError::Invalid(why) => Exchange::Refused(format!("{why}.")),
        TokenError::BadClient => {
            tracing::error!(
                "eBay refused this server's app credentials: check EBAY_CLIENT_ID, EBAY_CLIENT_SECRET and EBAY_RUNAME"
            );
            Exchange::Unavailable(
                "eBay refused this server's app credentials; ask the server's operator to check them".into(),
            )
        }
        TokenError::RateLimited(_) => {
            Exchange::Unavailable("eBay is limiting requests; try again in a minute".into())
        }
        TokenError::Unavailable(why) => Exchange::Unavailable(why),
    }
}

/// The verifier the service offers: people sign in through eBay. Switching eBay environment,
/// keyset or the scopes asked for signs everyone out.
pub fn verifier(client: Arc<EbayClient>) -> Arc<dyn CredentialVerifier> {
    let generation = format!(
        "{}\0{}\0{}",
        client.base().as_str(),
        client.tokens().app().client_id(),
        crate::oauth::user_scope()
    );
    Arc::new(
        UpstreamVerifier::new(LABEL, EbaySignIn::new(client))
            .with_help("Sign in with the eBay account you buy and sell with. Tools act as that account: they can list, change and end its listings and handle its orders.")
            .with_generation(generation.into_bytes()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refused_codes_say_why_and_app_problems_blame_the_server() {
        assert!(matches!(
            verdict(TokenError::Invalid("invalid_grant".into())),
            Exchange::Refused(m) if m == "invalid_grant."
        ));
        assert!(matches!(
            verdict(TokenError::BadClient),
            Exchange::Unavailable(m) if m.contains("operator")
        ));
        assert!(matches!(
            verdict(TokenError::RateLimited(Some(5))),
            Exchange::Unavailable(_)
        ));
    }
}
