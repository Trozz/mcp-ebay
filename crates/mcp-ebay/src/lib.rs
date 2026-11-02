//! The mcp-ebay server: eBay tools and prompts on one [`mcp_server_core::app::Server`], each
//! person signing in through eBay's own OAuth.

use mcp_server_core::app::{Server, SignIn};
pub use mcp_server_core::combined::COMBINED;
use mcp_server_core::service::ServiceFactory;

pub const VERSION: &str = match option_env!("APP_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

pub const SERVICE_NAME: &str = "mcp-ebay";
/// Tracing target prefix: also matches `mcp_ebay_marketplace` and every other `mcp_ebay_*` crate.
pub const CRATE_TARGET: &str = "mcp_ebay";

/// Every service this binary can run, in mount order. Adding a service: a library crate
/// implementing `mcp_server_core::service::Service`, a dependency in Cargo.toml and one line here.
pub const SERVICES: &[(&str, ServiceFactory)] = &[("ebay", mcp_ebay_marketplace::from_env)];

/// The server: `EBAY_SERVICES`, `EBAY_COMBINED` and `EBAY_READ_ONLY` choose what runs, and people
/// sign in with their own eBay account through eBay's OAuth, set up by the eBay service.
pub fn server() -> Server {
    Server::new(SERVICE_NAME, VERSION)
        .env_prefix("EBAY")
        .crate_target(CRATE_TARGET)
        .services(SERVICES)
        .sign_in(SignIn::ServiceCredential)
        .icon(
            include_bytes!("../assets/favicon.png"),
            include_str!("../assets/favicon.svg"),
        )
        .docs_url("https://github.com/Trozz/mcp-ebay#configuration")
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_server_core::service::ServiceEnv;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> ServiceEnv {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ServiceEnv::new(Box::new(move |k: &str| map.get(k).cloned()))
    }

    const CORE: &[(&str, &str)] = &[
        ("PUBLIC_URL", "https://ebay-mcp.example.com"),
        ("OAUTH_SIGNING_KEY", "0123456789abcdef0123456789abcdef"),
        ("EBAY_CLIENT_ID", "AnnLee-MyApp-PRD-1a2b3c4d5-6e7f8a9b"),
        ("EBAY_CLIENT_SECRET", "PRD-1a2b3c4d5e6f-7a8b"),
        ("EBAY_RUNAME", "Ann_Lee-AnnLee-MyApp-abcdefgh"),
    ];

    #[test]
    fn people_sign_in_through_ebay() {
        let mut e = env(CORE);
        let loaded = server().load(&mut e).expect("loads");
        assert!(e.errors.is_empty(), "{:?}", e.errors);
        assert_eq!(loaded.verifier.label(), "eBay account");
        assert!(loaded.verifier.carried());
        assert!(!loaded.verifier.accepts_bearer());
        assert_eq!(
            loaded.verifier.upstream().map(|u| u.provider()),
            Some("eBay")
        );
    }

    #[test]
    fn the_ebay_app_is_required_and_errors_are_reported_together() {
        let mut e = env(&[("PUBLIC_URL", "ftp://x")]);
        assert!(server().load(&mut e).is_none());
        let joined = e.errors.join("\n");
        for needle in [
            "PUBLIC_URL",
            "OAUTH_SIGNING_KEY is required",
            "EBAY_CLIENT_ID is required",
            "EBAY_CLIENT_SECRET is required",
            "EBAY_RUNAME is required",
        ] {
            assert!(joined.contains(needle), "{needle}: {joined}");
        }
    }

    #[test]
    fn a_static_token_is_refused_because_everyone_signs_in_themselves() {
        let mut e = env(&[CORE, &[("MCP_AUTH_TOKEN", "a-shared-token")]].concat());
        assert!(server().load(&mut e).is_none());
        assert!(
            e.errors.join("\n").contains("MCP_AUTH_TOKEN"),
            "{:?}",
            e.errors
        );
    }

    #[test]
    fn switching_ebay_environment_signs_everyone_out() {
        let version = |extra: &[(&str, &str)]| {
            let mut e = env(&[CORE, extra].concat());
            let loaded = server().load(&mut e).expect("loads");
            mcp_server_core::oauth::sign_in_version(
                b"0123456789abcdef0123456789abcdef",
                loaded.verifier.as_ref(),
            )
        };
        assert_ne!(version(&[]), version(&[("EBAY_ENVIRONMENT", "sandbox")]));
        assert_ne!(
            version(&[]),
            version(&[("EBAY_CLIENT_ID", "AnnLee-Other-PRD-1a2b3c4d5")])
        );
        assert_eq!(version(&[]), version(&[]));
    }

    #[test]
    fn read_only_hides_the_write_tools() {
        let mut e = env(&[CORE, &[("EBAY_READ_ONLY", "ebay")]].concat());
        let loaded = server().load(&mut e).expect("loads");
        assert!(loaded.safety.is_read_only("ebay"));
    }

    #[test]
    fn combined_tool_names_are_unique_and_fit_clients() {
        let mut e = env(CORE);
        let loaded = server().load(&mut e).expect("loads");
        let names = Server::combined_tool_names(&loaded.services);
        assert!(names.len() >= 20, "{}", names.len());
        for want in [
            "ebay_create_listing",
            "ebay_update_listing",
            "ebay_delete_listing",
            "ebay_search_items",
        ] {
            assert!(names.contains(&want.to_string()), "{want}: {names:?}");
        }
        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        for n in &names {
            assert!(
                n.len() <= 64
                    && n.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{n}"
            );
        }
    }

    #[test]
    fn debug_directive_covers_service_crates() {
        use tracing::Level;
        use tracing_subscriber::layer::SubscriberExt;
        let directives = mcp_server_core::observability::build_filter("info", true, CRATE_TARGET);
        let subscriber =
            tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::new(directives));
        tracing::subscriber::with_default(subscriber, || {
            assert!(tracing::enabled!(target: "mcp_ebay_marketplace::api", Level::DEBUG));
            assert!(tracing::enabled!(target: "mcp_ebay", Level::DEBUG));
            assert!(!tracing::enabled!(target: "hyper", Level::DEBUG));
        });
    }
}
