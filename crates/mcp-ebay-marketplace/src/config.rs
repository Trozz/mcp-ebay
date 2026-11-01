//! The eBay service's own settings: the eBay app (keyset) it signs people in with, which eBay it
//! talks to, and the marketplace tools default to. The server binary owns the core configuration
//! (URL, OAuth signing key, TLS, logging).

use mcp_server_core::config::EnvReader;
use secrecy::SecretString;
use url::Url;

/// MCP `serverInfo.name` of the eBay service.
pub const SERVICE_NAME: &str = "mcp-ebay-marketplace";

pub const CLIENT_ID_VAR: &str = "EBAY_CLIENT_ID";
pub const CLIENT_SECRET_VAR: &str = "EBAY_CLIENT_SECRET";
pub const RUNAME_VAR: &str = "EBAY_RUNAME";
pub const ENVIRONMENT_VAR: &str = "EBAY_ENVIRONMENT";
pub const MARKETPLACE_VAR: &str = "EBAY_MARKETPLACE";
pub const API_URL_VAR: &str = "EBAY_API_URL";
pub const IDENTITY_URL_VAR: &str = "EBAY_IDENTITY_URL";
pub const AUTH_URL_VAR: &str = "EBAY_AUTH_URL";

/// Which eBay: the live site or the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Production,
    Sandbox,
}

impl Environment {
    pub fn api_url(self) -> &'static str {
        match self {
            Environment::Production => "https://api.ebay.com",
            Environment::Sandbox => "https://api.sandbox.ebay.com",
        }
    }

    /// The host of the Identity API (`apiz`).
    pub fn identity_url(self) -> &'static str {
        match self {
            Environment::Production => "https://apiz.ebay.com",
            Environment::Sandbox => "https://apiz.sandbox.ebay.com",
        }
    }

    /// The page where people sign in to eBay and grant this app access.
    pub fn auth_url(self) -> &'static str {
        match self {
            Environment::Production => "https://auth.ebay.com/oauth2/authorize",
            Environment::Sandbox => "https://auth.sandbox.ebay.com/oauth2/authorize",
        }
    }

    pub fn is_sandbox(self) -> bool {
        self == Environment::Sandbox
    }
}

/// An eBay marketplace (site) and what goes with it.
#[derive(Debug, PartialEq, Eq)]
pub struct Marketplace {
    /// `EBAY_GB`, as the REST APIs name it.
    pub id: &'static str,
    /// `Content-Language` for listings written in this marketplace.
    pub language: &'static str,
    pub currency: &'static str,
    /// The Trading API's `X-EBAY-API-SITEID`.
    pub site_id: u32,
    /// The website, for listing links.
    pub domain: &'static str,
    /// ISO 3166 country code.
    pub country: &'static str,
}

const fn mp(
    id: &'static str,
    language: &'static str,
    currency: &'static str,
    site_id: u32,
    domain: &'static str,
    country: &'static str,
) -> Marketplace {
    Marketplace {
        id,
        language,
        currency,
        site_id,
        domain,
        country,
    }
}

/// The marketplaces this server knows how to list on.
pub const MARKETPLACES: &[Marketplace] = &[
    mp("EBAY_GB", "en-GB", "GBP", 3, "www.ebay.co.uk", "GB"),
    mp("EBAY_US", "en-US", "USD", 0, "www.ebay.com", "US"),
    mp("EBAY_DE", "de-DE", "EUR", 77, "www.ebay.de", "DE"),
    mp("EBAY_AU", "en-AU", "AUD", 15, "www.ebay.com.au", "AU"),
    mp("EBAY_CA", "en-CA", "CAD", 2, "www.ebay.ca", "CA"),
    mp("EBAY_FR", "fr-FR", "EUR", 71, "www.ebay.fr", "FR"),
    mp("EBAY_IT", "it-IT", "EUR", 101, "www.ebay.it", "IT"),
    mp("EBAY_ES", "es-ES", "EUR", 186, "www.ebay.es", "ES"),
    mp("EBAY_IE", "en-IE", "EUR", 205, "www.ebay.ie", "IE"),
    mp("EBAY_AT", "de-AT", "EUR", 16, "www.ebay.at", "AT"),
    mp("EBAY_NL", "nl-NL", "EUR", 146, "www.ebay.nl", "NL"),
];

/// The marketplace named `raw`: `EBAY_GB`, `GB`, `ebay_gb` or `uk`.
pub fn marketplace(raw: &str) -> Option<&'static Marketplace> {
    let t = raw.trim().to_ascii_uppercase().replace('-', "_");
    let t = match t.as_str() {
        "UK" | "EBAY_UK" => "EBAY_GB".to_string(),
        other if !other.starts_with("EBAY_") => format!("EBAY_{other}"),
        other => other.to_string(),
    };
    MARKETPLACES.iter().find(|m| m.id == t)
}

pub fn marketplace_ids() -> String {
    MARKETPLACES
        .iter()
        .map(|m| m.id)
        .collect::<Vec<_>>()
        .join(", ")
}

pub struct EbayConfig {
    /// The keyset's App ID (client id).
    pub client_id: String,
    /// The keyset's Cert ID (client secret).
    pub client_secret: SecretString,
    /// The RuName (eBay redirect URL name) whose auth accepted URL is `PUBLIC_URL/callback`.
    pub runame: String,
    pub environment: Environment,
    /// The marketplace tools use when none is given.
    pub marketplace: &'static Marketplace,
    /// `https://api.ebay.com`, or an override.
    pub api_url: Url,
    /// `https://apiz.ebay.com`, or an override.
    pub identity_url: Url,
    /// eBay's sign-in page, or an override.
    pub auth_url: Url,
}

impl EbayConfig {
    pub fn from_env<F: Fn(&str) -> Option<String>>(env: &mut EnvReader<F>) -> Option<Self> {
        let errors_before = env.errors.len();
        let client_id = env.required(CLIENT_ID_VAR);
        if let Some(id) = &client_id
            && !plausible_key(id)
        {
            env.errors.push(format!(
                "{CLIENT_ID_VAR} must be the App ID (Client ID) of your eBay keyset, such as Ann-MyApp-PRD-1a2b3c4d5-6e7f8a9b"
            ));
        }
        let client_secret = env.secret_required(CLIENT_SECRET_VAR);
        let runame = env.required(RUNAME_VAR);
        if let Some(r) = &runame
            && (!plausible_key(r) || r.contains("://"))
        {
            env.errors.push(format!(
                "{RUNAME_VAR} must be the RuName (eBay Redirect URL name) from User Tokens in the eBay developer portal, such as Ann_Lee-AnnLee-MyApp-abcdefgh, not a URL"
            ));
        }
        let environment = match env.optional(ENVIRONMENT_VAR).as_deref().map(str::trim) {
            None | Some("") | Some("production") | Some("live") => Environment::Production,
            Some("sandbox") => Environment::Sandbox,
            Some(other) => {
                env.errors.push(format!(
                    "{ENVIRONMENT_VAR} must be production or sandbox, got {other:?}"
                ));
                Environment::Production
            }
        };
        let market = match env.optional(MARKETPLACE_VAR) {
            None => marketplace("EBAY_GB"),
            Some(raw) => {
                let m = marketplace(&raw);
                if m.is_none() {
                    env.errors.push(format!(
                        "{MARKETPLACE_VAR} must be one of {}, got {raw:?}",
                        marketplace_ids()
                    ));
                }
                m
            }
        };
        let mut url = |var: &str, default: &str| -> Option<Url> {
            let raw = env.optional(var).unwrap_or_else(|| default.to_string());
            match parse_url(var, &raw) {
                Ok(u) => Some(u),
                Err(msg) => {
                    env.errors.push(msg);
                    None
                }
            }
        };
        let api_url = url(API_URL_VAR, environment.api_url());
        // With the API overridden (a test double or a proxy), the Identity API defaults to it too.
        let identity_default = api_url
            .as_ref()
            .filter(|u| u.as_str().trim_end_matches('/') != environment.api_url())
            .map(|u| u.as_str().trim_end_matches('/').to_string())
            .unwrap_or_else(|| environment.identity_url().to_string());
        let identity_url = url(IDENTITY_URL_VAR, &identity_default);
        let auth_url = url(AUTH_URL_VAR, environment.auth_url());
        if env.errors.len() != errors_before {
            return None;
        }
        Some(EbayConfig {
            client_id: client_id?,
            client_secret: client_secret?,
            runame: runame?.trim().to_string(),
            environment,
            marketplace: market?,
            api_url: api_url?,
            identity_url: identity_url?,
            auth_url: auth_url?,
        })
    }
}

/// An eBay App ID or RuName: letters, digits, `-`, `_` and `.`.
fn plausible_key(id: &str) -> bool {
    let id = id.trim();
    (6..=200).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A base URL from variable `name`: https, or http to a loopback address; no credentials, query
/// or fragment. A trailing slash is removed.
pub fn parse_url(name: &str, raw: &str) -> Result<Url, String> {
    let mut url = Url::parse(raw.trim()).map_err(|e| format!("{name} is not a valid URL: {e}"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(format!("{name} must not contain credentials"));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(format!("{name} must not contain a query or fragment"));
    }
    let ok = (url.scheme() == "https" && url.host_str().is_some())
        || (url.scheme() == "http" && is_loopback(&url));
    if !ok {
        return Err(format!(
            "{name} must use https (http is only allowed for loopback addresses)"
        ));
    }
    let path = url.path().trim_end_matches('/').to_string();
    url.set_path(&path);
    Ok(url)
}

pub(crate) fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;
    use std::collections::HashMap;

    const APP: &[(&str, &str)] = &[
        (CLIENT_ID_VAR, "AnnLee-MyApp-PRD-1a2b3c4d5-6e7f8a9b"),
        (CLIENT_SECRET_VAR, "PRD-1a2b3c4d5e6f-7a8b-9c0d"),
        (RUNAME_VAR, "Ann_Lee-AnnLee-MyApp-abcdefgh"),
    ];

    fn load(pairs: &[(&str, &str)]) -> (Option<EbayConfig>, Vec<String>) {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        let mut env = EnvReader::new(move |k: &str| map.get(k).cloned());
        let c = EbayConfig::from_env(&mut env);
        (c, env.errors)
    }

    #[test]
    fn defaults_to_live_ebay_uk() {
        let (c, errors) = load(APP);
        assert!(errors.is_empty(), "{errors:?}");
        let c = c.unwrap();
        assert_eq!(c.api_url.as_str(), "https://api.ebay.com/");
        assert_eq!(c.identity_url.as_str(), "https://apiz.ebay.com/");
        assert_eq!(
            c.auth_url.as_str(),
            "https://auth.ebay.com/oauth2/authorize"
        );
        assert_eq!(c.marketplace.id, "EBAY_GB");
        assert_eq!(
            c.client_secret.expose_secret(),
            "PRD-1a2b3c4d5e6f-7a8b-9c0d"
        );
        assert_eq!(c.environment, Environment::Production);
    }

    #[test]
    fn the_sandbox_another_marketplace_and_overrides() {
        let (c, errors) = load(
            &[
                APP,
                &[(ENVIRONMENT_VAR, "sandbox"), (MARKETPLACE_VAR, "us")],
            ]
            .concat(),
        );
        assert!(errors.is_empty(), "{errors:?}");
        let c = c.unwrap();
        assert_eq!(c.api_url.as_str(), "https://api.sandbox.ebay.com/");
        assert_eq!(c.identity_url.as_str(), "https://apiz.sandbox.ebay.com/");
        assert_eq!(c.marketplace.id, "EBAY_US");
        let (c, errors) = load(&[APP, &[(API_URL_VAR, "http://127.0.0.1:9999/")]].concat());
        assert!(errors.is_empty(), "{errors:?}");
        let c = c.unwrap();
        assert_eq!(c.api_url.as_str(), "http://127.0.0.1:9999/");
        assert_eq!(c.identity_url.as_str(), "http://127.0.0.1:9999/");
    }

    #[test]
    fn every_problem_is_reported() {
        let (c, errors) = load(&[
            (CLIENT_ID_VAR, "bad id!"),
            (RUNAME_VAR, "https://example.com/callback"),
            (ENVIRONMENT_VAR, "staging"),
            (MARKETPLACE_VAR, "EBAY_MARS"),
            (API_URL_VAR, "http://api.ebay.com"),
        ]);
        assert!(c.is_none());
        let joined = errors.join("\n");
        for needle in [
            CLIENT_ID_VAR,
            "EBAY_CLIENT_SECRET is required",
            "not a URL",
            "production or sandbox",
            "EBAY_MARS",
            "must use https",
        ] {
            assert!(joined.contains(needle), "{needle}: {joined}");
        }
    }

    #[test]
    fn marketplaces_are_found_by_any_spelling() {
        assert_eq!(marketplace("uk").unwrap().id, "EBAY_GB");
        assert_eq!(marketplace("GB").unwrap().id, "EBAY_GB");
        assert_eq!(marketplace(" ebay-de ").unwrap().currency, "EUR");
        assert!(marketplace("EBAY_XX").is_none());
    }

    #[test]
    fn reads_only_its_own_variables() {
        let read = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen = read.clone();
        let mut env = EnvReader::new(move |k: &str| {
            seen.lock().unwrap().push(k.to_string());
            None
        });
        let _ = EbayConfig::from_env(&mut env);
        for k in read.lock().unwrap().iter() {
            assert!(k.starts_with("EBAY_"), "read {k}");
            assert_ne!(k, "EBAY_SERVICES");
        }
    }
}
