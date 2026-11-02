//! Every live listing of the account, however it was made, through the Trading API: listings made
//! on the eBay website or in other tools have only an item number, not a SKU in the Inventory
//! API. Also ending a listing of either kind.

use super::inventory::offers_for;
use super::{
    EbayServer, choice, currency_arg, description_arg, id_arg, item_number, json_result, opt_money,
    run, sku_arg, title_arg,
};
use crate::api::{Call, trading_warnings};
use crate::error::{self, EbayError};
use crate::view::clean;
use crate::xml::{self, el, escape};
use axum::http::request::Parts;
use reqwest::Method;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ActiveParams {
    /// active (default): live listings; unsold: ended without selling, in the last 60 days; scheduled: not started yet.
    pub list: Option<String>,
    /// Page number, from 1. Default: 1.
    pub page: Option<u32>,
    /// 1 to 200. Default: 50.
    pub per_page: Option<u32>,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ReviseParams {
    /// The item number of the listing (from list_active_listings or its link).
    pub item_id: String,
    /// At most 80 characters. eBay may refuse title changes once an item has sold or has bids.
    pub title: Option<String>,
    /// The new description (HTML allowed); replaces the old one.
    pub description: Option<String>,
    /// The new Buy It Now price, or an auction's starting price before bids.
    pub price: Option<String>,
    pub currency: Option<String>,
    /// The new quantity available (fixed price).
    pub quantity: Option<u32>,
    /// Set the listing's SKU, e.g. so migrate_listings can move it.
    pub sku: Option<String>,
    /// The listing is an auction rather than fixed price. Default: false.
    pub auction: Option<bool>,
    /// e.g. EBAY_GB: the site the listing is on. Default: the server's.
    pub marketplace: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct EndParams {
    /// The SKU of a listing made through this server: its live offer is withdrawn, keeping the item and offer to publish again later.
    pub sku: Option<String>,
    /// The offer to withdraw, instead of sku.
    pub offer_id: Option<String>,
    /// The item number of any listing, such as one made on the eBay website: it is ended.
    pub item_id: Option<String>,
    /// For item_id: not_available (default), incorrect, lost_or_broken, other_listing_error, or sell_to_high_bidder (auctions with bids).
    pub reason: Option<String>,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
}

/// One listing of `GetMyeBaySelling` as tools show it.
fn listing(i: &Value) -> Value {
    let t = |p: &str| xml::text(i.pointer(p));
    let selling = i.get("SellingStatus");
    let quantity = t("/Quantity").and_then(|q| q.parse::<i64>().ok());
    let available = t("/QuantityAvailable").and_then(|q| q.parse::<i64>().ok());
    let sold = selling
        .and_then(|s| xml::text(s.get("QuantitySold")))
        .and_then(|q| q.parse::<i64>().ok());
    let price = xml::amount(selling.and_then(|s| s.get("CurrentPrice")))
        .or_else(|| xml::amount(i.get("BuyItNowPrice")))
        .or_else(|| xml::amount(i.get("StartPrice")));
    clean(json!({
        "item_id": t("/ItemID"),
        "title": t("/Title"),
        "sku": t("/SKU"),
        "format": match t("/ListingType").as_deref() {
            Some("FixedPriceItem") | Some("StoresFixedPrice") => Some("fixed_price".to_string()),
            Some("Chinese") => Some("auction".to_string()),
            other => other.map(String::from),
        },
        "price": crate::view::money(price.as_ref()),
        "buy_it_now_price": crate::view::money(xml::amount(i.get("BuyItNowPrice")).as_ref()).filter(|p| !p.starts_with("0.0 ")),
        "quantity": quantity,
        "available": available.or_else(|| quantity.zip(sold).map(|(q, s)| q - s)),
        "sold": sold,
        "bids": selling.and_then(|s| xml::text(s.get("BidCount"))),
        "watchers": t("/WatchCount"),
        "time_left": t("/TimeLeft"),
        "started": t("/ListingDetails/StartTime"),
        "ended": t("/ListingDetails/EndTime"),
        "url": t("/ListingDetails/ViewItemURL"),
        "photo": t("/PictureDetails/GalleryURL"),
    }))
}

const END_REASONS: &[(&str, &str)] = &[
    ("not_available", "NotAvailable"),
    ("incorrect", "Incorrect"),
    ("lost_or_broken", "LostOrBroken"),
    ("other_listing_error", "OtherListingError"),
    ("sell_to_high_bidder", "SellToHighBidder"),
];

fn end_reason(v: Option<&str>) -> error::Result<&'static str> {
    let names: Vec<&'static str> = END_REASONS.iter().map(|(n, _)| *n).collect();
    let chosen = choice("reason", v, &names)?.unwrap_or("not_available");
    Ok(END_REASONS
        .iter()
        .find(|(n, _)| *n == chosen)
        .map(|(_, r)| *r)
        .expect("a known reason"))
}

#[tool_router(router = listings_router, vis = "pub(crate)")]
impl EbayServer {
    #[tool(
        description = "Every live listing of the account (or those ended unsold, or scheduled), however it was made: item number, title, SKU, format, price, quantity available and sold, bids, watchers, time left and link. Use the item number with revise_listing, end_listing or migrate_listings.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn list_active_listings(
        &self,
        Parameters(p): Parameters<ActiveParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("list_active_listings", async {
            let c = self.caller(&parts)?;
            let m = c.marketplace(p.marketplace.as_deref())?;
            let page = p.page.unwrap_or(1);
            let per_page = p.per_page.unwrap_or(50);
            if page == 0 || page > 10_000 || !(1..=200).contains(&per_page) {
                return Err(EbayError::invalid("page must be 1 or more and per_page 1 to 200"));
            }
            let container = match choice("list", p.list.as_deref(), &["active", "unsold", "scheduled"])? {
                None | Some("active") => "ActiveList",
                Some("unsold") => "UnsoldList",
                _ => "ScheduledList",
            };
            let inner = format!(
                "<{container}><Include>true</Include><Pagination><EntriesPerPage>{per_page}</EntriesPerPage><PageNumber>{page}</PageNumber></Pagination></{container}><HideVariations>false</HideVariations>"
            );
            let v = c.trading("GetMyeBaySelling", m, &inner).await?;
            let list = v.get(container);
            let items: Vec<Value> = xml::list(list.and_then(|l| l.pointer("/ItemArray/Item")))
                .into_iter()
                .map(listing)
                .collect();
            let total = list
                .and_then(|l| xml::text(l.pointer("/PaginationResult/TotalNumberOfEntries")))
                .and_then(|t| t.parse::<u64>().ok());
            let pages = list
                .and_then(|l| xml::text(l.pointer("/PaginationResult/TotalNumberOfPages")))
                .and_then(|t| t.parse::<u32>().ok());
            let mut out = json!({
                "marketplace": m.id,
                "list": container,
                "count": items.len(),
                "total": total,
                "page": page,
                "items": items,
            });
            if pages.is_some_and(|n| page < n) {
                out["next_page"] = Value::from(page + 1);
            }
            Ok(json_result(&clean(out)))
        })
        .await
    }

    #[tool(
        description = "Change a live listing by item number, such as one made on the eBay website: its title, description, price, quantity or SKU. Only what is given changes. Listings made through this server (with a SKU from create_listing) are changed with update_listing instead.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn revise_listing(
        &self,
        Parameters(p): Parameters<ReviseParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("revise_listing", async {
            let c = self.caller(&parts)?;
            let m = c.marketplace(p.marketplace.as_deref())?;
            let id = item_number("item_id", &p.item_id)?;
            let currency = currency_arg(p.currency.as_deref(), m)?;
            let mut item = el("ItemID", &id);
            let mut changed = Vec::new();
            if let Some(t) = title_arg(p.title.as_deref())? {
                item.push_str(&el("Title", &t));
                changed.push("title");
            }
            if let Some(d) = description_arg(p.description.as_deref())? {
                item.push_str(&el("Description", &d));
                changed.push("description");
            }
            if let Some(pr) = opt_money("price", p.price.as_deref())? {
                item.push_str(&format!(
                    "<StartPrice currencyID=\"{}\">{}</StartPrice>",
                    escape(&currency),
                    escape(&pr)
                ));
                changed.push("price");
            }
            if let Some(q) = p.quantity {
                if p.auction == Some(true) {
                    return Err(EbayError::invalid("an auction's quantity cannot be changed"));
                }
                item.push_str(&el("Quantity", &q.to_string()));
                changed.push("quantity");
            }
            if let Some(s) = &p.sku {
                item.push_str(&el("SKU", &sku_arg(s)?));
                changed.push("sku");
            }
            if changed.is_empty() {
                return Err(EbayError::invalid("give at least one thing to change"));
            }
            let call = if p.auction == Some(true) { "ReviseItem" } else { "ReviseFixedPriceItem" };
            let v = c
                .trading(call, m, &format!("<Item>{item}</Item>"))
                .await
                .map_err(|e| match e {
                    EbayError::Validation(msg) if msg.contains("21919474") || msg.to_ascii_lowercase().contains("inventory") => {
                        EbayError::Validation(format!("{msg}. If this listing was made through this server, change it by SKU with update_listing"))
                    }
                    other => other,
                })?;
            Ok(json_result(&clean(json!({
                "ok": true,
                "item_id": xml::text(v.get("ItemID")).unwrap_or(id.clone()),
                "changed": changed,
                "url": c.listing_url(m, &id),
                "warnings": trading_warnings(&v),
            }))))
        })
        .await
    }

    #[tool(
        description = "End a live listing early. With sku or offer_id (listings made through this server) the offer is withdrawn: the listing ends but the item and offer stay, to publish again with publish_listing. With item_id (any listing, such as one made on the eBay website) the listing is ended with a reason. Auctions with bids may only end if the account allows it.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn end_listing(
        &self,
        Parameters(p): Parameters<EndParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("end_listing", async {
            let c = self.caller(&parts)?;
            let m = c.marketplace(p.marketplace.as_deref())?;
            let given = [p.sku.is_some(), p.offer_id.is_some(), p.item_id.is_some()]
                .iter()
                .filter(|g| **g)
                .count();
            if given != 1 {
                return Err(EbayError::invalid(
                    "give exactly one of sku, offer_id and item_id",
                ));
            }
            if let Some(raw) = &p.item_id {
                let id = item_number("item_id", raw)?;
                let reason = end_reason(p.reason.as_deref())?;
                let v = c
                    .trading(
                        "EndItem",
                        m,
                        &format!("{}{}", el("ItemID", &id), el("EndingReason", reason)),
                    )
                    .await?;
                return Ok(json_result(&clean(json!({
                    "ok": true,
                    "item_id": id,
                    "ended": xml::text(v.get("EndTime")),
                    "warnings": trading_warnings(&v),
                }))));
            }
            let offers: Vec<String> = match (&p.offer_id, &p.sku) {
                (Some(o), _) => vec![id_arg("offer_id", o)?],
                (_, Some(s)) => {
                    let sku = sku_arg(s)?;
                    let live: Vec<String> = offers_for(&c, &sku, Some(m))
                        .await?
                        .iter()
                        .filter(|o| o.get("status").and_then(Value::as_str) == Some("PUBLISHED"))
                        .filter_map(|o| o.get("offerId").and_then(Value::as_str).map(String::from))
                        .collect();
                    if live.is_empty() {
                        return Err(EbayError::NotFound(format!(
                            "SKU {sku} has no live listing on {}",
                            m.id
                        )));
                    }
                    live
                }
                _ => unreachable!("one of them was given"),
            };
            let mut ended = Vec::new();
            for id in &offers {
                let a = c
                    .send(Call::new(
                        Method::POST,
                        "withdraw_offer",
                        &["sell", "inventory", "v1", "offer", id, "withdraw"],
                    ))
                    .await?;
                ended.push(clean(
                    json!({"offer_id": id, "listing_id": a.body.get("listingId")}),
                ));
            }
            Ok(json_result(&json!({"ok": true, "withdrawn": ended})))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_map_to_trading_names() {
        assert_eq!(end_reason(None).unwrap(), "NotAvailable");
        assert_eq!(end_reason(Some("lost or broken")).unwrap(), "LostOrBroken");
        assert!(end_reason(Some("bored")).is_err());
    }

    #[test]
    fn listings_are_read_from_trading_xml() {
        let v = xml::parse(r#"<Item><ItemID>123456789012</ItemID><Title>Lamp</Title><SKU>L-1</SKU>
            <ListingType>FixedPriceItem</ListingType><Quantity>5</Quantity>
            <SellingStatus><CurrentPrice currencyID="GBP">20.0</CurrentPrice><QuantitySold>2</QuantitySold></SellingStatus>
            <ListingDetails><ViewItemURL>https://www.ebay.co.uk/itm/123456789012</ViewItemURL></ListingDetails>
            <WatchCount>3</WatchCount></Item>"#).unwrap();
        let l = listing(&v);
        assert_eq!(l["item_id"], "123456789012");
        assert_eq!(l["format"], "fixed_price");
        assert_eq!(l["price"], "20.0 GBP");
        assert_eq!(l["available"], 3);
        assert_eq!(l["sold"], 2);
        assert_eq!(l["watchers"], "3");
    }
}
