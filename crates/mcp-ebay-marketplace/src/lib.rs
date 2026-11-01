//! eBay as MCP tools and prompts: searching eBay, and managing the signed-in seller's listings,
//! policies, inventory locations and orders, through eBay's REST APIs and, for listings made on
//! the eBay website, the Trading API.

pub mod api;
pub mod config;
pub mod error;
pub mod oauth;
pub mod prompts;
pub mod service;
pub mod tools;
pub mod verifier;
pub mod view;
pub mod xml;

pub use service::{EbayService, from_env};

pub const VERSION: &str = match option_env!("APP_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};
