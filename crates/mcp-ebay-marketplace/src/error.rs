#[derive(Debug, thiserror::Error)]
pub enum EbayError {
    /// eBay no longer accepts this sign-in: the refresh token was revoked or expired, or the
    /// person withdrew this app's access.
    #[error(
        "eBay no longer accepts this connection's sign-in (it may have been revoked or expired). Sign in to eBay again: reconnect the connector, or run /mcp in Claude Code"
    )]
    Auth,
    /// Signed in, but eBay does not allow this for the account or the app.
    #[error("{0}")]
    Permission(String),
    #[error("this request carried no eBay sign-in; connect again")]
    MissingCredential,
    /// A complete message: tool errors are already prefixed with the kind, `not_found: `.
    #[error("{0}")]
    NotFound(String),
    #[error("{}", rate_limited_message(*.retry_after_secs))]
    RateLimited {
        /// Seconds eBay asked us to wait, when it said.
        retry_after_secs: Option<u64>,
    },
    #[error("{0}")]
    Validation(String),
    #[error("eBay error: {0}")]
    Upstream(String),
    #[error("{0}")]
    Internal(String),
}

fn rate_limited_message(retry_after_secs: Option<u64>) -> String {
    let base =
        "the eBay API call limit for this app or account is used up (most limits reset daily)";
    match retry_after_secs {
        Some(s) => format!("{base}; try again in {s} seconds"),
        None => format!("{base}; try again later"),
    }
}

impl EbayError {
    pub fn kind(&self) -> &'static str {
        match self {
            EbayError::Auth | EbayError::MissingCredential => "auth",
            EbayError::Permission(_) => "permission",
            EbayError::NotFound(_) => "not_found",
            EbayError::RateLimited { .. } => "rate_limited",
            EbayError::Validation(_) => "validation",
            EbayError::Upstream(_) => "upstream",
            EbayError::Internal(_) => "internal",
        }
    }

    pub fn invalid(msg: impl Into<String>) -> Self {
        EbayError::Validation(msg.into())
    }
}

pub type Result<T> = std::result::Result<T, EbayError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        for (e, kind) in [
            (EbayError::Auth, "auth"),
            (EbayError::MissingCredential, "auth"),
            (EbayError::Permission("x".into()), "permission"),
            (EbayError::NotFound("x".into()), "not_found"),
            (
                EbayError::RateLimited {
                    retry_after_secs: None,
                },
                "rate_limited",
            ),
            (EbayError::invalid("x"), "validation"),
            (EbayError::Upstream("x".into()), "upstream"),
            (EbayError::Internal("x".into()), "internal"),
        ] {
            assert_eq!(e.kind(), kind);
        }
    }

    #[test]
    fn messages_say_what_to_do() {
        assert!(
            EbayError::Auth
                .to_string()
                .contains("Sign in to eBay again")
        );
        let m = EbayError::RateLimited {
            retry_after_secs: Some(30),
        }
        .to_string();
        assert!(m.contains("30 seconds"), "{m}");
    }
}
