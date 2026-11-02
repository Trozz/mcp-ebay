//! Searching eBay: items for sale from every seller, through the Browse API with the application
//! token.

use super::{
    EbayServer, choice, currency_arg, id_arg, json_result, opt_money, paging, run, text_arg,
};
use crate::api::Call;
use crate::error::{self, EbayError};
use crate::view::{clean, html_text, money, page};
use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

/// Longest description `get_item` returns, in characters.
const MAX_DESCRIPTION: usize = 6000;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SearchParams {
    /// Words to search for, e.g. "lego 75192". Give this, category_id or gtin.
    pub query: Option<String>,
    /// Only items in this category (from suggest_categories).
    pub category_id: Option<String>,
    /// A barcode (EAN, UPC or ISBN) to find exactly this product.
    pub gtin: Option<String>,
    /// Lowest price, e.g. "10".
    pub min_price: Option<String>,
    /// Highest price, e.g. "50.00".
    pub max_price: Option<String>,
    /// Currency of min_price and max_price. Default: the marketplace's.
    pub currency: Option<String>,
    /// new, used or unspecified. For finer conditions use condition_ids.
    pub condition: Option<String>,
    /// Condition ids, e.g. ["1000", "3000"] (new, used excellent); get_category_requirements lists them.
    pub condition_ids: Option<Vec<String>>,
    /// fixed_price (Buy It Now), auction or best_offer.
    pub buying_option: Option<String>,
    /// Only items located in this country (ISO code, e.g. GB).
    pub item_location_country: Option<String>,
    /// Only items with free postage.
    pub free_shipping: Option<bool>,
    /// Only items whose seller accepts returns.
    pub returns_accepted: Option<bool>,
    /// Only items from these sellers (usernames).
    pub sellers: Option<Vec<String>>,
    /// Any other Browse API filter, e.g. "searchInDescription:true" or "itemEndDate:[..2026-12-01T00:00:00Z]".
    pub filter: Option<String>,
    /// best_match (default), price (lowest first, with postage), -price (highest first), newly_listed or ending_soonest.
    pub sort: Option<String>,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
    /// Postcode to estimate delivery and postage to.
    pub delivery_postcode: Option<String>,
    /// 1 to 200. Default: 20.
    pub limit: Option<u32>,
    /// Results to skip: pass next_offset for the next page.
    pub offset: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ItemParams {
    /// The item id from search_items (v1|123456789012|0), or the item number from a listing link (123456789012).
    pub item_id: String,
    /// For an item number with variations: the variation's id.
    pub variation_id: Option<String>,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
}

/// `{a|b}` with each value checked: eBay filter values cannot hold `{`, `}`, `|` or `,`.
fn filter_set(name: &str, values: &[String]) -> error::Result<String> {
    let mut clean_values = Vec::new();
    for v in values {
        let t = v.trim();
        if t.is_empty() || t.contains(['{', '}', '|', ',']) || t.len() > 100 {
            return Err(EbayError::invalid(format!(
                "values of {name} cannot be empty or contain {{, }}, | or ,"
            )));
        }
        clean_values.push(t.to_string());
    }
    Ok(format!("{{{}}}", clean_values.join("|")))
}

/// An item summary as search shows it.
fn summary(i: &Value) -> Value {
    let shipping = i
        .get("shippingOptions")
        .and_then(Value::as_array)
        .and_then(|s| s.first());
    let location = [
        i.pointer("/itemLocation/postalCode"),
        i.pointer("/itemLocation/country"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .collect::<Vec<_>>()
    .join(" ");
    let feedback = i.pointer("/seller/feedbackPercentage").map(|p| {
        format!(
            "{}%",
            p.as_str()
                .map(String::from)
                .unwrap_or_else(|| p.to_string())
        )
    });
    let postage = shipping.and_then(|s| {
        if s.get("shippingCostType").and_then(Value::as_str) == Some("CALCULATED") {
            Some("calculated".to_string())
        } else {
            money(s.get("shippingCost"))
        }
    });
    clean(json!({
        "item_id": i.get("itemId"),
        "item_number": i.get("legacyItemId"),
        "title": i.get("title"),
        "price": money(i.get("price")),
        "current_bid": money(i.get("currentBidPrice")),
        "bids": i.get("bidCount"),
        "buying_options": i.get("buyingOptions"),
        "condition": i.get("condition"),
        "postage": postage,
        "seller": i.pointer("/seller/username"),
        "seller_feedback": feedback,
        "seller_feedback_score": i.pointer("/seller/feedbackScore"),
        "location": location,
        "ends": i.get("itemEndDate"),
        "listed": i.get("itemCreationDate"),
        "top_rated": i.get("topRatedBuyingExperience"),
        "image": i.pointer("/image/imageUrl"),
        "url": i.get("itemWebUrl"),
    }))
}

/// One item as get_item shows it.
fn detail(i: &Value) -> Value {
    let aspects: serde_json::Map<String, Value> = i
        .get("localizedAspects")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    Some((
                        x.get("name")?.as_str()?.to_string(),
                        x.get("value")?.clone(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut images: Vec<Value> = i.pointer("/image/imageUrl").cloned().into_iter().collect();
    if let Some(more) = i.get("additionalImages").and_then(Value::as_array) {
        images.extend(more.iter().filter_map(|x| x.get("imageUrl").cloned()));
    }
    let shipping: Vec<Value> = i
        .get("shippingOptions")
        .and_then(Value::as_array)
        .map(|s| {
            s.iter()
                .map(|o| {
                    clean(json!({
                        "service": o.get("shippingServiceCode"),
                        "carrier": o.get("shippingCarrierCode"),
                        "cost": money(o.get("shippingCost")),
                        "type": o.get("shippingCostType"),
                        "delivery_from": o.get("minEstimatedDeliveryDate"),
                        "delivery_to": o.get("maxEstimatedDeliveryDate"),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    let available = i
        .get("estimatedAvailabilities")
        .and_then(Value::as_array)
        .and_then(|a| a.first());
    clean(json!({
        "item_id": i.get("itemId"),
        "item_number": i.get("legacyItemId"),
        "title": i.get("title"),
        "subtitle": i.get("subtitle"),
        "price": money(i.get("price")),
        "current_bid": money(i.pointer("/currentBidPrice")),
        "bids": i.get("bidCount"),
        "buying_options": i.get("buyingOptions"),
        "condition": i.get("condition"),
        "condition_id": i.get("conditionId"),
        "condition_description": i.get("conditionDescription"),
        "category": i.get("categoryPath"),
        "category_id": i.get("categoryId"),
        "brand": i.get("brand"),
        "mpn": i.get("mpn"),
        "gtin": i.get("gtin"),
        "item_specifics": aspects,
        "short_description": i.get("shortDescription"),
        "description": i.get("description").and_then(Value::as_str).map(|d| html_text(d, MAX_DESCRIPTION)),
        "images": images,
        "available": available.and_then(|a| a.get("estimatedAvailableQuantity")),
        "sold": available.and_then(|a| a.get("estimatedSoldQuantity")),
        "seller": i.pointer("/seller/username"),
        "seller_feedback": i.pointer("/seller/feedbackPercentage"),
        "seller_feedback_score": i.pointer("/seller/feedbackScore"),
        "location": i.get("itemLocation"),
        "shipping": shipping,
        "returns": i.get("returnTerms").map(|r| clean(json!({
            "accepted": r.get("returnsAccepted"),
            "within": r.pointer("/returnPeriod/value").map(|v| format!("{} {}", v, r.pointer("/returnPeriod/unit").and_then(Value::as_str).unwrap_or("DAY"))),
            "postage_paid_by": r.get("returnShippingCostPayer"),
        }))),
        "ends": i.get("itemEndDate"),
        "url": i.get("itemWebUrl"),
    }))
}

#[tool_router(router = browse_router, vis = "pub(crate)")]
impl EbayServer {
    #[tool(
        description = "Search eBay for items for sale, from every seller: by words, category or barcode, filtered by price, condition, format (Buy It Now, auction, best offer), location, free postage and seller, and sorted by best match, price, newest or ending soonest. Each result has its item_id (for get_item), price, postage, condition, seller and link. Use it to research what similar items sell for.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn search_items(
        &self,
        Parameters(p): Parameters<SearchParams>,
        Extension(_parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("search_items", async {
            let m = self.marketplace(p.marketplace.as_deref())?;
            let (limit, offset) = paging(p.limit, p.offset, 20, 200)?;
            if offset + limit > 10_000 {
                return Err(EbayError::invalid(
                    "eBay returns at most the first 10000 results; narrow the search",
                ));
            }
            let query = text_arg("query", p.query.as_deref())?;
            let category = p
                .category_id
                .as_deref()
                .map(|c| id_arg("category_id", c))
                .transpose()?;
            let gtin = match text_arg("gtin", p.gtin.as_deref())? {
                Some(g) if (8..=14).contains(&g.len()) && g.chars().all(|c| c.is_ascii_digit()) => {
                    Some(g)
                }
                Some(_) => {
                    return Err(EbayError::invalid(
                        "gtin must be an EAN, UPC or ISBN of 8 to 14 digits",
                    ));
                }
                None => None,
            };
            if query.is_none() && category.is_none() && gtin.is_none() {
                return Err(EbayError::invalid("give query, category_id or gtin"));
            }
            let mut filters: Vec<String> = Vec::new();
            let min = opt_money("min_price", p.min_price.as_deref())?;
            let max = opt_money("max_price", p.max_price.as_deref())?;
            if min.is_some() || max.is_some() {
                filters.push(format!(
                    "price:[{}..{}]",
                    min.unwrap_or_default(),
                    max.unwrap_or_default()
                ));
                filters.push(format!(
                    "priceCurrency:{}",
                    currency_arg(p.currency.as_deref(), m)?
                ));
            }
            if let Some(cond) = choice(
                "condition",
                p.condition.as_deref(),
                &["NEW", "USED", "UNSPECIFIED"],
            )? {
                filters.push(format!("conditions:{{{cond}}}"));
            }
            if let Some(ids) = p.condition_ids.as_deref().filter(|v| !v.is_empty()) {
                if ids
                    .iter()
                    .any(|i| !i.trim().chars().all(|c| c.is_ascii_digit()))
                {
                    return Err(EbayError::invalid(
                        "condition_ids must be numbers such as 1000",
                    ));
                }
                filters.push(format!(
                    "conditionIds:{}",
                    filter_set("condition_ids", ids)?
                ));
            }
            if let Some(b) = choice(
                "buying_option",
                p.buying_option.as_deref(),
                &["FIXED_PRICE", "AUCTION", "BEST_OFFER"],
            )? {
                filters.push(format!("buyingOptions:{{{b}}}"));
            }
            if let Some(cc) = text_arg("item_location_country", p.item_location_country.as_deref())?
            {
                if cc.len() != 2 || !cc.chars().all(|c| c.is_ascii_alphabetic()) {
                    return Err(EbayError::invalid(
                        "item_location_country must be a two-letter country code such as GB",
                    ));
                }
                filters.push(format!("itemLocationCountry:{}", cc.to_ascii_uppercase()));
            }
            if p.free_shipping == Some(true) {
                filters.push("maxDeliveryCost:0".into());
            }
            if p.returns_accepted == Some(true) {
                filters.push("returnsAccepted:true".into());
            }
            if let Some(s) = p.sellers.as_deref().filter(|v| !v.is_empty()) {
                filters.push(format!("sellers:{}", filter_set("sellers", s)?));
            }
            if let Some(extra) = text_arg("filter", p.filter.as_deref())? {
                filters.push(extra);
            }
            let sort = match choice(
                "sort",
                p.sort.as_deref(),
                &[
                    "best_match",
                    "price",
                    "-price",
                    "newly_listed",
                    "ending_soonest",
                ],
            ) {
                // `choice` ignores the - of -price; tell them apart here.
                Ok(Some("price"))
                    if p.sort.as_deref().is_some_and(|s| s.trim().starts_with('-')) =>
                {
                    Some("-price")
                }
                Ok(Some("best_match")) | Ok(None) => None,
                Ok(Some("newly_listed")) => Some("newlyListed"),
                Ok(Some("ending_soonest")) => Some("endingSoonest"),
                Ok(other) => other,
                Err(e) => return Err(e),
            };
            let mut call = Call::get("search", &["buy", "browse", "v1", "item_summary", "search"])
                .marketplace(m.id)
                .query_opt("q", query)
                .query_opt("category_ids", category)
                .query_opt("gtin", gtin)
                .query_opt("sort", sort)
                .query("limit", limit.to_string())
                .query("offset", offset.to_string());
            if !filters.is_empty() {
                call = call.query("filter", filters.join(","));
            }
            if let Some(pc) = text_arg("delivery_postcode", p.delivery_postcode.as_deref())? {
                let pc = pc.replace(',', " ");
                call = call.header(
                    "X-EBAY-C-ENDUSERCTX",
                    format!(
                        "contextualLocation=country%3D{}%2Czip%3D{}",
                        m.country,
                        url_encode(&pc)
                    ),
                );
            }
            let a = self.public(call).await?;
            let items: Vec<Value> = a
                .body
                .get("itemSummaries")
                .and_then(Value::as_array)
                .map(|v| v.iter().map(summary).collect())
                .unwrap_or_default();
            let mut out = page(items, offset, a.body.get("total").and_then(Value::as_u64));
            out["marketplace"] = Value::from(m.id);
            if let Some(w) = a.body.get("warnings") {
                out["warnings"] = w.clone();
            }
            Ok(json_result(&out))
        })
        .await
    }

    #[tool(
        description = "One item for sale on eBay in full: price, postage options, condition and its description, item specifics, the seller's description as text, photos, quantity available and sold, seller, returns and link. Takes an item_id from search_items or an item number.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn get_item(
        &self,
        Parameters(p): Parameters<ItemParams>,
        Extension(_parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("get_item", async {
            let m = self.marketplace(p.marketplace.as_deref())?;
            let raw = p.item_id.trim();
            let number = super::item_number("item_id", raw).ok();
            let call = match number {
                Some(n) => Call::get(
                    "item_by_legacy_id",
                    &["buy", "browse", "v1", "item", "get_item_by_legacy_id"],
                )
                .query("legacy_item_id", n)
                .query_opt(
                    "legacy_variation_id",
                    p.variation_id
                        .as_deref()
                        .map(|v| id_arg("variation_id", v))
                        .transpose()?,
                ),
                None => {
                    let id = id_arg("item_id", raw)?;
                    Call::get("item", &["buy", "browse", "v1", "item", &id])
                }
            };
            let a = self.public(call.marketplace(m.id)).await?;
            Ok(json_result(&detail(&a.body)))
        })
        .await
    }
}

/// Percent-encodes `s` for a header value.
fn url_encode(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_sets_refuse_separators() {
        assert_eq!(filter_set("s", &["a".into(), "b".into()]).unwrap(), "{a|b}");
        assert!(filter_set("s", &["a,b".into()]).is_err());
    }

    #[test]
    fn summaries_are_compact() {
        let s = summary(&json!({
            "itemId": "v1|1|0", "legacyItemId": "1", "title": "LEGO",
            "price": {"value": "10.00", "currency": "GBP"},
            "shippingOptions": [{"shippingCostType": "FIXED", "shippingCost": {"value": "0.00", "currency": "GBP"}}],
            "seller": {"username": "bob", "feedbackPercentage": "99.5", "feedbackScore": 10},
            "itemLocation": {"postalCode": "SW1", "country": "GB"},
            "itemWebUrl": "https://www.ebay.co.uk/itm/1"
        }));
        assert_eq!(s["price"], "10.00 GBP");
        assert_eq!(s["postage"], "0.00 GBP");
        assert_eq!(s["seller_feedback"], "99.5%");
        assert_eq!(s["location"], "SW1 GB");
    }
}
