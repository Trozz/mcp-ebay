use crate::api::{ApiSettings, EbayClient, http_client};
use crate::config::{EbayConfig, Marketplace};
use crate::oauth::{OAuthApp, Tokens};
use crate::tools::EbayServer;
use mcp_server_core::combined::DynHandler;
use mcp_server_core::credential::CredentialVerifier;
use mcp_server_core::service::{Service, ServiceContext, ServiceEnv, mcp_service};
use std::sync::Arc;

/// The eBay service, mounted by the server at `/ebay`.
pub struct EbayService {
    client: Arc<EbayClient>,
    marketplace: &'static Marketplace,
}

impl EbayService {
    pub fn new(client: Arc<EbayClient>, marketplace: &'static Marketplace) -> Self {
        Self {
            client,
            marketplace,
        }
    }

    fn server(&self) -> EbayServer {
        EbayServer::new(self.client.clone(), self.marketplace)
    }
}

impl Service for EbayService {
    fn name(&self) -> &'static str {
        "ebay"
    }

    fn tool_names(&self) -> Vec<String> {
        self.server().tool_names()
    }

    fn router(self: Arc<Self>, ctx: &ServiceContext) -> axum::Router {
        tracing::info!(
            api = %self.client.base(),
            client_id = %self.client.tokens().app().client_id(),
            runame = %self.client.tokens().app().runame(),
            marketplace = self.marketplace.id,
            "ebay settings"
        );
        mcp_service(ctx, move || Ok(self.server()))
    }

    fn credential_verifier(&self) -> Option<Arc<dyn CredentialVerifier>> {
        Some(crate::verifier::verifier(self.client.clone()))
    }

    fn handler(&self) -> DynHandler {
        DynHandler::new(self.server())
    }
}

/// Builds the eBay client from its parts: the eBay app, the token cache and the APIs.
pub fn client(cfg: &EbayConfig) -> Result<EbayClient, String> {
    let settings = ApiSettings::new(cfg.api_url.clone(), cfg.identity_url.clone());
    let http = http_client(&settings).map_err(|e| e.to_string())?;
    let app = Arc::new(OAuthApp::new(
        http.clone(),
        cfg.api_url.clone(),
        cfg.auth_url.clone(),
        cfg.client_id.clone(),
        cfg.client_secret.clone(),
        cfg.runame.clone(),
    ));
    Ok(EbayClient::new(
        http,
        settings,
        Tokens::new(app),
        cfg.environment.is_sandbox(),
    ))
}

/// Reads the eBay configuration and builds the service. Errors go to `env.errors`.
pub fn from_env(env: &mut ServiceEnv) -> Option<Arc<dyn Service>> {
    let cfg = EbayConfig::from_env(env)?;
    match client(&cfg) {
        Ok(c) => Some(Arc::new(EbayService::new(Arc::new(c), cfg.marketplace))),
        Err(e) => {
            env.errors.push(format!("eBay client setup failed: {e}"));
            None
        }
    }
}
