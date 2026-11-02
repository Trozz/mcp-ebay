//! Listings through the Inventory API: an inventory item (the product: title, description,
//! photos, condition, item specifics and quantity, named by SKU) and an offer (price, category,
//! policies and marketplace) that is published as a live listing.

use super::account::{FULFILLMENT, PAYMENT, RETURN, choose_location, choose_policy};
use super::{
    Caller, EbayServer, amount, aspects_arg, choice, condition_arg, currency_arg, deep_merge,
    description_arg, fields, id_arg, image_urls_arg, item_number, json_result, money_arg,
    opt_money, opt_s, run, sku_arg, text_arg, title_arg,
};
use crate::api::{Call, error_detail};
use crate::config::Marketplace;
use crate::error::{self, EbayError};
use crate::view::{clean, html_text, money};
use axum::http::request::Parts;
use reqwest::Method;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// Longest description `get_listing` shows as text.
const MAX_DESCRIPTION_TEXT: usize = 4000;
/// Most SKUs in one price and quantity update, as eBay allows.
const MAX_BULK: usize = 25;
/// Most listings migrated at once, as eBay allows.
const MAX_MIGRATE: usize = 5;
const AUCTION_DAYS: &[u32] = &[1, 3, 5, 7, 10];

fn inventory_item_path(sku: &str) -> Vec<&str> {
    vec!["sell", "inventory", "v1", "inventory_item", sku]
}

/// The offers of `sku`, on marketplace `m` when given.
pub(crate) async fn offers_for(
    c: &Caller,
    sku: &str,
    m: Option<&Marketplace>,
) -> error::Result<Vec<Value>> {
    let call = Call::get("offers", &["sell", "inventory", "v1", "offer"])
        .query("sku", sku)
        .query_opt("marketplace_id", m.map(|m| m.id));
    // eBay answers a SKU without offers with a 404, or a 400 naming error 25713 (offer not
    // available) or 25702 (SKU not found).
    let a = match c.send(call).await {
        Ok(a) => a,
        Err(EbayError::NotFound(_)) => return Ok(Vec::new()),
        Err(EbayError::Validation(m))
            if m.contains("[error 25713]") || m.contains("[error 25702]") =>
        {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e),
    };
    Ok(a.body
        .get("offers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn offer_id(o: &Value) -> Option<String> {
    o.get("offerId").and_then(Value::as_str).map(String::from)
}

fn is_published(o: &Value) -> bool {
    o.get("status").and_then(Value::as_str) == Some("PUBLISHED")
}

/// The one offer of `sku` a tool acts on: `given`, or the only one on marketplace `m`.
async fn the_offer(
    c: &Caller,
    sku: &str,
    m: &Marketplace,
    given: Option<&str>,
) -> error::Result<Value> {
    let offers = offers_for(c, sku, None).await?;
    if let Some(g) = given.map(str::trim).filter(|g| !g.is_empty()) {
        let g = id_arg("offer_id", g)?;
        return offers
            .into_iter()
            .find(|o| offer_id(o).as_deref() == Some(g.as_str()))
            .ok_or_else(|| EbayError::NotFound(format!("SKU {sku} has no offer {g}")));
    }
    let mine: Vec<Value> = offers
        .into_iter()
        .filter(|o| o.get("marketplaceId").and_then(Value::as_str) == Some(m.id))
        .collect();
    match mine.len() {
        0 => Err(EbayError::NotFound(format!(
            "SKU {sku} has no offer on {}; create_listing makes one",
            m.id
        ))),
        1 => Ok(mine.into_iter().next().expect("one")),
        _ => Err(EbayError::invalid(format!(
            "SKU {sku} has several offers on {}; give offer_id as one of {}",
            m.id,
            mine.iter()
                .filter_map(offer_id)
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// An offer as tools show it.
fn offer_summary(sandbox: bool, o: &Value) -> Value {
    let m = o
        .get("marketplaceId")
        .and_then(Value::as_str)
        .and_then(crate::config::marketplace);
    let listing_id = o.pointer("/listing/listingId").and_then(Value::as_str);
    clean(json!({
        "offer_id": o.get("offerId"),
        "status": o.get("status"),
        "marketplace": o.get("marketplaceId"),
        "format": o.get("format"),
        "price": money(o.pointer("/pricingSummary/price")),
        "auction_start_price": money(o.pointer("/pricingSummary/auctionStartPrice")),
        "reserve_price": money(o.pointer("/pricingSummary/auctionReservePrice")),
        "available_quantity": o.get("availableQuantity"),
        "category_id": o.get("categoryId"),
        "duration": o.get("listingDuration"),
        "listing_id": listing_id,
        "listing_status": o.pointer("/listing/listingStatus"),
        "sold_quantity": o.pointer("/listing/soldQuantity"),
        "url": match (listing_id, m) {
            (Some(id), Some(m)) => Some(super::listing_url(sandbox, m, id)),
            _ => None,
        },
        "fulfillment_policy_id": o.pointer("/listingPolicies/fulfillmentPolicyId"),
        "payment_policy_id": o.pointer("/listingPolicies/paymentPolicyId"),
        "return_policy_id": o.pointer("/listingPolicies/returnPolicyId"),
        "best_offer": o.pointer("/listingPolicies/bestOfferTerms/bestOfferEnabled"),
        "merchant_location_key": o.get("merchantLocationKey"),
    }))
}

/// An inventory item as tools show it.
fn item_summary(i: &Value, full: bool) -> Value {
    let images = i.pointer("/product/imageUrls").and_then(Value::as_array);
    let mut out = json!({
        "sku": i.get("sku"),
        "title": i.pointer("/product/title"),
        "condition": i.get("condition"),
        "condition_description": i.get("conditionDescription"),
        "quantity": i.pointer("/availability/shipToLocationAvailability/quantity"),
    });
    if full {
        let d = i.pointer("/product/description").and_then(Value::as_str);
        out["description_text"] = Value::from(d.map(|d| html_text(d, MAX_DESCRIPTION_TEXT)));
        out["item_specifics"] = i
            .pointer("/product/aspects")
            .cloned()
            .unwrap_or(Value::Null);
        out["photos"] = images.cloned().map(Value::Array).unwrap_or(Value::Null);
        for k in ["brand", "mpn", "ean", "upc", "isbn", "epid"] {
            out[k] = i
                .pointer(&format!("/product/{k}"))
                .cloned()
                .unwrap_or(Value::Null);
        }
        out["package"] = i
            .get("packageWeightAndSize")
            .cloned()
            .unwrap_or(Value::Null);
    } else {
        out["photo_count"] = Value::from(images.map_or(0, Vec::len));
    }
    clean(out)
}

/// Fields of the inventory item or offer that eBay sets itself and refuses back in an update.
const OFFER_READ_ONLY: &[&str] = &[
    "offerId",
    "sku",
    "marketplaceId",
    "format",
    "status",
    "listing",
];
const ITEM_READ_ONLY: &[&str] = &["sku", "locale", "groupIds", "inventoryItemGroupKeys"];

fn strip(mut v: Value, keys: &[&str]) -> Value {
    if let Some(m) = v.as_object_mut() {
        for k in keys {
            m.remove(*k);
        }
    }
    v
}

/// The product fields of an `extra_product` or `extra_offer` argument, refusing what the tool
/// sets itself.
fn extra(name: &str, v: Option<&Map<String, Value>>, forbidden: &[&str]) -> error::Result<Value> {
    let m = v.cloned().unwrap_or_default();
    for k in forbidden {
        if m.contains_key(*k) {
            return Err(EbayError::invalid(format!("{name} must not contain {k}")));
        }
    }
    Ok(Value::Object(m))
}

/// A SKU made up for a new listing: `MCP-` and the time in base 36, with random letters.
fn new_sku() -> String {
    use ring::rand::SecureRandom;
    let mut r = [0u8; 3];
    let _ = ring::rand::SystemRandom::new().fill(&mut r);
    let mut n = chrono::Utc::now().timestamp_millis() as u64;
    let digits = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut s = Vec::new();
    while n > 0 {
        s.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    s.reverse();
    let tail: String = r
        .iter()
        .map(|b| digits[(*b % 36) as usize] as char)
        .collect();
    format!("MCP-{}-{tail}", String::from_utf8_lossy(&s))
}

fn gtin_list(name: &str, v: Option<&str>) -> error::Result<Option<Value>> {
    let Some(t) = text_arg(name, v)? else {
        return Ok(None);
    };
    let digits: String = t
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .collect();
    let ok = (8..=14).contains(&digits.len())
        && digits
            .chars()
            .all(|c| c.is_ascii_digit() || (name == "isbn" && c == 'X'));
    if !ok {
        return Err(EbayError::invalid(format!("{name} must be 8 to 14 digits")));
    }
    Ok(Some(json!([digits])))
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateListingParams {
    /// The listing title, at most 80 characters. Put the most searched words first.
    pub title: String,
    /// The description buyers see; HTML is allowed.
    pub description: String,
    /// The leaf category id, from suggest_categories.
    pub category_id: String,
    /// The condition, e.g. NEW, USED_EXCELLENT, USED_GOOD, FOR_PARTS_OR_NOT_WORKING, or its id (1000); get_category_requirements lists those the category allows.
    pub condition: String,
    /// Details of a used item's condition, e.g. "Light scratches on the lid".
    pub condition_description: Option<String>,
    /// The price, e.g. "24.99"; for an auction, the starting bid.
    pub price: String,
    /// Default: the marketplace's currency.
    pub currency: Option<String>,
    /// How many are for sale. Default: 1. Auctions are always 1.
    pub quantity: Option<u32>,
    /// fixed_price (Buy It Now, the default; runs until sold or ended) or auction.
    pub format: Option<String>,
    /// For an auction: 1, 3, 5, 7 (default) or 10 days.
    pub auction_days: Option<u32>,
    /// For an auction: a Buy It Now price as well.
    pub buy_it_now_price: Option<String>,
    /// For an auction: the lowest price it sells for.
    pub reserve_price: Option<String>,
    /// Item specifics, e.g. {"Brand": "LEGO", "Colour": ["Red", "Blue"]}. Include every required one from get_category_requirements.
    pub aspects: Option<Map<String, Value>>,
    /// Photos: public https URLs, the first is the main photo; at most 24. Most categories need at least one.
    pub image_urls: Option<Vec<String>>,
    /// Your own reference for the item (letters, digits, -, _ and .; at most 50). Default: one is made up.
    pub sku: Option<String>,
    pub brand: Option<String>,
    /// Manufacturer part number.
    pub mpn: Option<String>,
    /// Barcode: EAN.
    pub ean: Option<String>,
    /// Barcode: UPC.
    pub upc: Option<String>,
    pub isbn: Option<String>,
    /// Allow buyers to make offers (fixed price only).
    pub best_offer: Option<bool>,
    /// Accept offers at or above this automatically.
    pub best_offer_auto_accept: Option<String>,
    /// Decline offers below this automatically.
    pub best_offer_auto_decline: Option<String>,
    /// Shipping policy id from get_seller_setup. Default: the only or the default one.
    pub fulfillment_policy_id: Option<String>,
    /// Payment policy id. Default: the only or the default one.
    pub payment_policy_id: Option<String>,
    /// Return policy id. Default: the only or the default one.
    pub return_policy_id: Option<String>,
    /// Inventory location key. Default: the only one.
    pub merchant_location_key: Option<String>,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
    /// Publish it as a live listing now. Default: true; false leaves it as an unpublished draft for publish_listing.
    pub publish: Option<bool>,
    /// Other inventory item fields by eBay's name, merged in, e.g. {"packageWeightAndSize": {"weight": {"value": 1.2, "unit": "KILOGRAM"}}}.
    pub extra_item: Option<Map<String, Value>>,
    /// Other offer fields by eBay's name, merged in, e.g. {"quantityLimitPerBuyer": 2, "storeCategoryNames": ["/Toys"]}.
    pub extra_offer: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ListItemsParams {
    /// Page number, from 1. Default: 1.
    pub page: Option<u32>,
    /// Items per page, 1 to 100. Default: 25.
    pub per_page: Option<u32>,
    /// Also show each item's offers (price and listing status): one more request per item. Default: false.
    pub include_offers: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SkuParams {
    /// The SKU.
    pub sku: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct UpdateListingParams {
    /// The SKU of the listing to change.
    pub sku: String,
    /// Which offer, when the SKU has several on the marketplace.
    pub offer_id: Option<String>,
    pub title: Option<String>,
    /// The new description (HTML allowed); replaces the old one.
    pub description: Option<String>,
    pub condition: Option<String>,
    pub condition_description: Option<String>,
    /// Item specifics to set, e.g. {"Colour": "Red"}; the others are kept.
    pub aspects: Option<Map<String, Value>>,
    /// Item specifics to remove, by name.
    pub remove_aspects: Option<Vec<String>>,
    /// The photos, replacing the old ones: public https URLs, main photo first.
    pub image_urls: Option<Vec<String>>,
    /// How many are for sale.
    pub quantity: Option<u32>,
    /// The new price (for an auction not yet bid on, the starting bid).
    pub price: Option<String>,
    pub currency: Option<String>,
    pub category_id: Option<String>,
    pub best_offer: Option<bool>,
    pub best_offer_auto_accept: Option<String>,
    pub best_offer_auto_decline: Option<String>,
    pub fulfillment_policy_id: Option<String>,
    pub payment_policy_id: Option<String>,
    pub return_policy_id: Option<String>,
    pub merchant_location_key: Option<String>,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
    /// Other inventory item fields by eBay's name, merged in.
    pub extra_item: Option<Map<String, Value>>,
    /// Other offer fields by eBay's name, merged in.
    pub extra_offer: Option<Map<String, Value>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct PriceQuantity {
    pub sku: String,
    /// The new price.
    pub price: Option<String>,
    /// The new quantity for sale; 0 hides a Buy It Now listing until it is restocked (when the account allows out-of-stock listings).
    pub quantity: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct BulkParams {
    /// Up to 25 SKUs with their new price and/or quantity.
    pub items: Vec<PriceQuantity>,
    /// Currency of the prices. Default: the marketplace's.
    pub currency: Option<String>,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct PublishParams {
    /// The SKU whose unpublished offer to publish. Give this or offer_id.
    pub sku: Option<String>,
    pub offer_id: Option<String>,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct MigrateParams {
    /// Up to 5 item numbers of listings made on the eBay website (from list_active_listings). Each needs a SKU, and the account must use business policies.
    pub item_ids: Vec<String>,
}

/// Best offer terms from the arguments, when any is given.
fn best_offer_terms(
    enabled: Option<bool>,
    accept: Option<&str>,
    decline: Option<&str>,
    currency: &str,
) -> error::Result<Option<Value>> {
    let accept = opt_money("best_offer_auto_accept", accept)?;
    let decline = opt_money("best_offer_auto_decline", decline)?;
    if enabled.is_none() && accept.is_none() && decline.is_none() {
        return Ok(None);
    }
    let on = enabled.unwrap_or(true);
    let mut t = json!({"bestOfferEnabled": on});
    if on {
        if let Some(a) = accept {
            t["autoAcceptPrice"] = amount(&a, currency);
        }
        if let Some(d) = decline {
            t["autoDeclinePrice"] = amount(&d, currency);
        }
    }
    Ok(Some(t))
}

/// Publishes offer `id`, returning the listing id and eBay's warnings.
async fn publish(c: &Caller, id: &str) -> error::Result<(String, Value)> {
    let a = c
        .send(Call::new(
            Method::POST,
            "publish_offer",
            &["sell", "inventory", "v1", "offer", id, "publish"],
        ))
        .await?;
    let listing = a
        .body
        .get("listingId")
        .and_then(Value::as_str)
        .ok_or_else(|| EbayError::Upstream("eBay published the offer but named no listing".into()))?
        .to_string();
    let warnings = a
        .body
        .get("warnings")
        .map(|w| Value::from(error_detail(&json!({"errors": w}).to_string())))
        .unwrap_or(Value::Null);
    Ok((listing, warnings))
}

#[tool_router(router = inventory_router, vis = "pub(crate)")]
impl EbayServer {
    #[tool(
        description = "List an item on eBay in one step: creates the inventory item (title, description, photos, condition, item specifics, quantity), its offer (price, category, policies) and publishes it, returning the SKU, offer id, listing id and link. Policies and location default to the account's only or default ones. Use suggest_categories and get_category_requirements first, and confirm the details with the person. With publish false it stays an unpublished draft. If eBay refuses to publish, the item and offer are kept: fix them with update_listing and call publish_listing.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub async fn create_listing(
        &self,
        Parameters(p): Parameters<CreateListingParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("create_listing", async {
            let c = self.caller(&parts)?;
            let m = c.marketplace(p.marketplace.as_deref())?;
            let title = title_arg(Some(&p.title))?
                .ok_or_else(|| EbayError::invalid("title must not be empty"))?;
            let description = description_arg(Some(&p.description))?
                .ok_or_else(|| EbayError::invalid("description must not be empty"))?;
            let category = id_arg("category_id", &p.category_id)?;
            let condition = condition_arg(Some(&p.condition))?
                .ok_or_else(|| EbayError::invalid("condition must not be empty"))?;
            let currency = currency_arg(p.currency.as_deref(), m)?;
            let price = money_arg("price", &p.price)?;
            let auction = choice("format", p.format.as_deref(), &["FIXED_PRICE", "AUCTION"])?
                == Some("AUCTION");
            let quantity = if auction {
                if p.quantity.is_some_and(|q| q != 1) {
                    return Err(EbayError::invalid("an auction is for one item: leave out quantity"));
                }
                1
            } else {
                p.quantity.unwrap_or(1)
            };
            if quantity == 0 {
                return Err(EbayError::invalid("quantity must be at least 1"));
            }
            let sku = match &p.sku {
                Some(s) => sku_arg(s)?,
                None => new_sku(),
            };
            let images = image_urls_arg(p.image_urls.as_deref())?;
            let aspects = aspects_arg(p.aspects.as_ref())?;
            let t = |n: &str, v: &Option<String>| -> error::Result<Option<Value>> {
                Ok(opt_s(text_arg(n, v.as_deref())?))
            };
            let product = fields(vec![
                ("title", Some(Value::from(title.clone()))),
                ("description", Some(Value::from(description))),
                ("aspects", aspects),
                ("imageUrls", images.clone().map(Value::from)),
                ("brand", t("brand", &p.brand)?),
                ("mpn", t("mpn", &p.mpn)?),
                ("ean", gtin_list("ean", p.ean.as_deref())?),
                ("upc", gtin_list("upc", p.upc.as_deref())?),
                ("isbn", gtin_list("isbn", p.isbn.as_deref())?),
            ]);
            let mut item = Value::Object(fields(vec![
                ("product", Some(Value::Object(product))),
                ("condition", Some(Value::from(condition))),
                (
                    "conditionDescription",
                    t("condition_description", &p.condition_description)?,
                ),
                (
                    "availability",
                    Some(json!({"shipToLocationAvailability": {"quantity": quantity}})),
                ),
            ]));
            deep_merge(
                &mut item,
                &extra("extra_item", p.extra_item.as_ref(), &["sku", "condition", "availability"])?,
            );
            let mut pricing = Map::new();
            if auction {
                let days = p.auction_days.unwrap_or(7);
                if !AUCTION_DAYS.contains(&days) {
                    return Err(EbayError::invalid("auction_days must be 1, 3, 5, 7 or 10"));
                }
                pricing.insert("auctionStartPrice".into(), amount(&price, &currency));
                if let Some(r) = opt_money("reserve_price", p.reserve_price.as_deref())? {
                    pricing.insert("auctionReservePrice".into(), amount(&r, &currency));
                }
                if let Some(b) = opt_money("buy_it_now_price", p.buy_it_now_price.as_deref())? {
                    pricing.insert("price".into(), amount(&b, &currency));
                }
            } else {
                if p.reserve_price.is_some() || p.buy_it_now_price.is_some() || p.auction_days.is_some() {
                    return Err(EbayError::invalid(
                        "reserve_price, buy_it_now_price and auction_days are for auctions: set format auction",
                    ));
                }
                pricing.insert("price".into(), amount(&price, &currency));
            }
            let best_offer = best_offer_terms(
                p.best_offer,
                p.best_offer_auto_accept.as_deref(),
                p.best_offer_auto_decline.as_deref(),
                &currency,
            )?;
            if auction && best_offer.is_some() {
                return Err(EbayError::invalid("best offers are for fixed price listings only"));
            }
            let fulfillment = choose_policy(&c, FULFILLMENT, m, p.fulfillment_policy_id.as_deref()).await?;
            let payment = choose_policy(&c, PAYMENT, m, p.payment_policy_id.as_deref()).await?;
            let returns = choose_policy(&c, RETURN, m, p.return_policy_id.as_deref()).await?;
            let location = choose_location(&c, p.merchant_location_key.as_deref()).await?;
            let mut policies = json!({
                "fulfillmentPolicyId": fulfillment,
                "paymentPolicyId": payment,
                "returnPolicyId": returns,
            });
            if let Some(b) = best_offer {
                policies["bestOfferTerms"] = b;
            }
            let format = if auction { "AUCTION" } else { "FIXED_PRICE" };
            let mut offer = json!({
                "sku": sku,
                "marketplaceId": m.id,
                "format": format,
                "availableQuantity": quantity,
                "categoryId": category,
                "listingPolicies": policies,
                "pricingSummary": pricing,
                "merchantLocationKey": location,
                "listingDuration": if auction { format!("DAYS_{}", p.auction_days.unwrap_or(7)) } else { "GTC".into() },
            });
            deep_merge(
                &mut offer,
                &extra("extra_offer", p.extra_offer.as_ref(), &["sku", "marketplaceId", "format"])?,
            );

            // An offer already made for this SKU: a live one is not listed twice, a draft is reused.
            let existing = if p.sku.is_some() {
                offers_for(&c, &sku, Some(m)).await?
            } else {
                Vec::new()
            };
            if let Some(live) = existing.iter().find(|o| is_published(o)) {
                return Err(EbayError::invalid(format!(
                    "SKU {sku} is already listed on {} (offer {}); change it with update_listing, or choose another sku",
                    m.id,
                    offer_id(live).unwrap_or_default()
                )));
            }
            let draft = existing
                .iter()
                .find(|o| o.get("format").and_then(Value::as_str) == Some(format))
                .and_then(offer_id);

            c.send(
                Call::new(Method::PUT, "put_inventory_item", &inventory_item_path(&sku))
                    .language(m.language)
                    .body(item),
            )
            .await?;
            let offer_id = match draft {
                Some(id) => {
                    c.send(
                        Call::new(Method::PUT, "update_offer", &["sell", "inventory", "v1", "offer", &id])
                            .language(m.language)
                            .body(strip(offer, OFFER_READ_ONLY)),
                    )
                    .await?;
                    id
                }
                None => {
                    let a = c
                        .send(
                            Call::new(Method::POST, "create_offer", &["sell", "inventory", "v1", "offer"])
                                .language(m.language)
                                .body(offer),
                        )
                        .await
                        .map_err(|e| match e {
                            EbayError::Validation(msg) => EbayError::Validation(format!(
                                "the inventory item {sku} was saved, but eBay refused its offer: {msg}"
                            )),
                            other => other,
                        })?;
                    a.body
                        .get("offerId")
                        .and_then(Value::as_str)
                        .ok_or_else(|| EbayError::Upstream("eBay created the offer but named no id".into()))?
                        .to_string()
                }
            };
            let mut out = json!({
                "sku": sku,
                "offer_id": offer_id,
                "marketplace": m.id,
                "title": title,
            });
            if images.as_ref().is_none_or(|i| i.is_empty()) {
                out["note"] = Value::from("no photos were given; most categories need at least one to publish");
            }
            if !p.publish.unwrap_or(true) {
                out["status"] = Value::from("UNPUBLISHED");
                out["next"] = Value::from("publish it with publish_listing when ready");
                return Ok(json_result(&out));
            }
            match publish(&c, &offer_id).await {
                Ok((listing, warnings)) => {
                    out["status"] = Value::from("PUBLISHED");
                    out["listing_id"] = Value::from(listing.clone());
                    out["url"] = Value::from(c.listing_url(m, &listing));
                    out["warnings"] = warnings;
                    Ok(json_result(&clean(out)))
                }
                Err(EbayError::Validation(msg)) => Err(EbayError::Validation(format!(
                    "the item and offer were saved (sku {sku}, offer {offer_id}) but eBay refused to publish: {msg}. Fix it with update_listing, then call publish_listing"
                ))),
                Err(e) => Err(e),
            }
        })
        .await
    }

    #[tool(
        description = "The account's inventory items (listings made through this server or migrated), by SKU: title, condition, quantity and photo count, and with include_offers their price and listing status. Listings made on the eBay website are in list_active_listings instead.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn list_inventory_items(
        &self,
        Parameters(p): Parameters<ListItemsParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("list_inventory_items", async {
            let c = self.caller(&parts)?;
            let page = p.page.unwrap_or(1);
            let per_page = p.per_page.unwrap_or(25);
            if page == 0 || page > 10_000 || !(1..=100).contains(&per_page) {
                return Err(EbayError::invalid(
                    "page must be 1 or more and per_page 1 to 100",
                ));
            }
            // getInventoryItems' offset is a page index from 0.
            let a = c
                .send(
                    Call::get(
                        "inventory_items",
                        &["sell", "inventory", "v1", "inventory_item"],
                    )
                    .query("limit", per_page.to_string())
                    .query("offset", (page - 1).to_string()),
                )
                .await?;
            let raw = a
                .body
                .get("inventoryItems")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut items = Vec::new();
            for i in &raw {
                let mut s = item_summary(i, false);
                if p.include_offers.unwrap_or(false)
                    && let Some(sku) = i.get("sku").and_then(Value::as_str)
                {
                    let offers = offers_for(&c, sku, None).await?;
                    s["offers"] = Value::from(
                        offers
                            .iter()
                            .map(|o| offer_summary(self.client.is_sandbox(), o))
                            .collect::<Vec<_>>(),
                    );
                }
                items.push(s);
            }
            let total = a.body.get("total").and_then(Value::as_u64);
            let mut out = json!({
                "count": items.len(),
                "total": total,
                "page": page,
                "items": items,
            });
            if total.is_some_and(|t| (page as u64) * (per_page as u64) < t) {
                out["next_page"] = Value::from(page + 1);
            }
            Ok(json_result(&clean(out)))
        })
        .await
    }

    #[tool(
        description = "One listing by SKU in full: the inventory item (title, description as text, condition, quantity, item specifics, photos, barcodes) and its offers (price, status, listing id and link, category, policies).",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn get_listing(
        &self,
        Parameters(p): Parameters<SkuParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("get_listing", async {
            let c = self.caller(&parts)?;
            let sku = sku_arg(&p.sku)?;
            let item = c
                .send(Call::get("inventory_item", &inventory_item_path(&sku)))
                .await
                .map_err(not_in_inventory(&sku))?;
            let offers = offers_for(&c, &sku, None).await?;
            let mut out = item_summary(&item.body, true);
            out["offers"] = Value::from(
                offers
                    .iter()
                    .map(|o| offer_summary(self.client.is_sandbox(), o))
                    .collect::<Vec<_>>(),
            );
            Ok(json_result(&clean(out)))
        })
        .await
    }

    #[tool(
        description = "Change a listing by SKU: title, description, condition, item specifics, photos, quantity, price, category, best offer or policies. Only what is given changes. A published listing is updated live on eBay. For listings made on the eBay website (no SKU in this server), use revise_listing.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn update_listing(
        &self,
        Parameters(p): Parameters<UpdateListingParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("update_listing", async {
            let c = self.caller(&parts)?;
            let m = c.marketplace(p.marketplace.as_deref())?;
            let sku = sku_arg(&p.sku)?;
            let currency = currency_arg(p.currency.as_deref(), m)?;
            let title = title_arg(p.title.as_deref())?;
            let description = description_arg(p.description.as_deref())?;
            let condition = condition_arg(p.condition.as_deref())?;
            let condition_description =
                text_arg("condition_description", p.condition_description.as_deref())?;
            let aspects = aspects_arg(p.aspects.as_ref())?;
            let images = image_urls_arg(p.image_urls.as_deref())?;
            let price = opt_money("price", p.price.as_deref())?;
            let category = p
                .category_id
                .as_deref()
                .map(|v| id_arg("category_id", v))
                .transpose()?;
            let best_offer = best_offer_terms(
                p.best_offer,
                p.best_offer_auto_accept.as_deref(),
                p.best_offer_auto_decline.as_deref(),
                &currency,
            )?;
            let policy =
                |n: &str, v: &Option<String>| v.as_deref().map(|x| id_arg(n, x)).transpose();
            let fulfillment = policy("fulfillment_policy_id", &p.fulfillment_policy_id)?;
            let payment = policy("payment_policy_id", &p.payment_policy_id)?;
            let returns = policy("return_policy_id", &p.return_policy_id)?;
            let location = policy("merchant_location_key", &p.merchant_location_key)?;
            let extra_item = extra("extra_item", p.extra_item.as_ref(), &["sku"])?;
            let extra_offer = extra(
                "extra_offer",
                p.extra_offer.as_ref(),
                &["sku", "marketplaceId", "format"],
            )?;
            let mut changed: Vec<&str> = Vec::new();

            let item_change = title.is_some()
                || description.is_some()
                || condition.is_some()
                || condition_description.is_some()
                || aspects.is_some()
                || p.remove_aspects.as_ref().is_some_and(|r| !r.is_empty())
                || images.is_some()
                || p.quantity.is_some()
                || extra_item.as_object().is_some_and(|o| !o.is_empty());
            let offer_change = price.is_some()
                || p.quantity.is_some()
                || category.is_some()
                || best_offer.is_some()
                || fulfillment.is_some()
                || payment.is_some()
                || returns.is_some()
                || location.is_some()
                || description.is_some()
                || extra_offer.as_object().is_some_and(|o| !o.is_empty());
            if !item_change && !offer_change {
                return Err(EbayError::invalid("give at least one thing to change"));
            }

            if item_change {
                let a = c
                    .send(Call::get("inventory_item", &inventory_item_path(&sku)))
                    .await
                    .map_err(not_in_inventory(&sku))?;
                let mut item = strip(a.body, ITEM_READ_ONLY);
                let product = item
                    .as_object_mut()
                    .ok_or_else(|| {
                        EbayError::Upstream("eBay's inventory item was not an object".into())
                    })?
                    .entry("product")
                    .or_insert_with(|| json!({}));
                if let Some(t) = &title {
                    product["title"] = Value::from(t.clone());
                    changed.push("title");
                }
                if let Some(d) = &description {
                    product["description"] = Value::from(d.clone());
                    changed.push("description");
                }
                if let Some(i) = &images {
                    product["imageUrls"] = Value::from(i.clone());
                    changed.push("photos");
                }
                if aspects.is_some() || p.remove_aspects.is_some() {
                    let mut current = product
                        .get("aspects")
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    for name in p.remove_aspects.iter().flatten() {
                        let n = name.trim();
                        let key = current.keys().find(|k| k.eq_ignore_ascii_case(n)).cloned();
                        if let Some(k) = key {
                            current.remove(&k);
                        }
                    }
                    if let Some(Value::Object(new)) = &aspects {
                        for (k, v) in new {
                            let old = current.keys().find(|x| x.eq_ignore_ascii_case(k)).cloned();
                            if let Some(o) = old {
                                current.remove(&o);
                            }
                            current.insert(k.clone(), v.clone());
                        }
                    }
                    product["aspects"] = Value::Object(current);
                    changed.push("item specifics");
                }
                if let Some(cnd) = condition {
                    item["condition"] = Value::from(cnd);
                    changed.push("condition");
                }
                if let Some(cd) = &condition_description {
                    item["conditionDescription"] = Value::from(cd.clone());
                    changed.push("condition description");
                }
                if let Some(q) = p.quantity {
                    item["availability"]["shipToLocationAvailability"]["quantity"] = Value::from(q);
                    changed.push("quantity");
                }
                if extra_item.as_object().is_some_and(|o| !o.is_empty()) {
                    deep_merge(&mut item, &extra_item);
                    changed.push("extra item fields");
                }
                c.send(
                    Call::new(
                        Method::PUT,
                        "put_inventory_item",
                        &inventory_item_path(&sku),
                    )
                    .language(m.language)
                    .body(item),
                )
                .await?;
            }

            let mut out = json!({"sku": sku});
            let offer_needed = price.is_some()
                || category.is_some()
                || best_offer.is_some()
                || fulfillment.is_some()
                || payment.is_some()
                || returns.is_some()
                || location.is_some()
                || extra_offer.as_object().is_some_and(|o| !o.is_empty());
            if offer_change {
                let found = match the_offer(&c, &sku, m, p.offer_id.as_deref()).await {
                    Ok(o) => Some(o),
                    // Only item fields were asked for and there is no offer yet: nothing else to do.
                    Err(EbayError::NotFound(_)) if !offer_needed => None,
                    Err(e) => return Err(e),
                };
                if let Some(o) = found {
                    let id = offer_id(&o)
                        .ok_or_else(|| EbayError::Upstream("an offer without an id".into()))?;
                    let published = is_published(&o);
                    let listing = o
                        .pointer("/listing/listingId")
                        .and_then(Value::as_str)
                        .map(String::from);
                    let mut offer = strip(o.clone(), OFFER_READ_ONLY);
                    let mut offer_touched = false;
                    if let Some(pr) = &price {
                        let key = if o.get("format").and_then(Value::as_str) == Some("AUCTION") {
                            "auctionStartPrice"
                        } else {
                            "price"
                        };
                        offer["pricingSummary"][key] = amount(pr, &currency);
                        changed.push("price");
                        offer_touched = true;
                    }
                    if let Some(q) = p.quantity {
                        offer["availableQuantity"] = Value::from(q);
                        offer_touched = true;
                    }
                    if let Some(cat) = &category {
                        offer["categoryId"] = Value::from(cat.clone());
                        changed.push("category");
                        offer_touched = true;
                    }
                    // An offer's own listing description overrides the item's: keep them the same.
                    if let Some(d) = &description
                        && offer.get("listingDescription").is_some()
                    {
                        offer["listingDescription"] = Value::from(d.clone());
                        offer_touched = true;
                    }
                    if let Some(b) = best_offer {
                        offer["listingPolicies"]["bestOfferTerms"] = b;
                        changed.push("best offer");
                        offer_touched = true;
                    }
                    for (key, v, label) in [
                        ("fulfillmentPolicyId", &fulfillment, "shipping policy"),
                        ("paymentPolicyId", &payment, "payment policy"),
                        ("returnPolicyId", &returns, "return policy"),
                    ] {
                        if let Some(v) = v {
                            offer["listingPolicies"][key] = Value::from(v.clone());
                            changed.push(label);
                            offer_touched = true;
                        }
                    }
                    if let Some(l) = &location {
                        offer["merchantLocationKey"] = Value::from(l.clone());
                        changed.push("location");
                        offer_touched = true;
                    }
                    if extra_offer.as_object().is_some_and(|o| !o.is_empty()) {
                        deep_merge(&mut offer, &extra_offer);
                        changed.push("extra offer fields");
                        offer_touched = true;
                    }
                    if offer_touched {
                        c.send(
                            Call::new(
                                Method::PUT,
                                "update_offer",
                                &["sell", "inventory", "v1", "offer", &id],
                            )
                            .language(m.language)
                            .body(offer),
                        )
                        .await?;
                    }
                    out["offer_id"] = Value::from(id);
                    out["published"] = Value::from(published);
                    if let Some(l) = listing {
                        out["url"] = Value::from(c.listing_url(m, &l));
                        out["listing_id"] = Value::from(l);
                    }
                }
            }
            changed.dedup();
            out["changed"] = Value::from(changed);
            Ok(json_result(&clean(out)))
        })
        .await
    }

    #[tool(
        description = "Change the price and/or quantity of up to 25 listings at once, by SKU. Live listings are updated on eBay. Returns each SKU's outcome.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn update_price_quantity(
        &self,
        Parameters(p): Parameters<BulkParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("update_price_quantity", async {
            let c = self.caller(&parts)?;
            let m = c.marketplace(p.marketplace.as_deref())?;
            let currency = currency_arg(p.currency.as_deref(), m)?;
            if p.items.is_empty() || p.items.len() > MAX_BULK {
                return Err(EbayError::invalid(format!("give 1 to {MAX_BULK} items")));
            }
            let mut requests = Vec::new();
            for it in &p.items {
                let sku = sku_arg(&it.sku)?;
                let price = opt_money("price", it.price.as_deref())?;
                if price.is_none() && it.quantity.is_none() {
                    return Err(EbayError::invalid(format!("give a price or quantity for {sku}")));
                }
                let offers: Vec<Value> = offers_for(&c, &sku, Some(m))
                    .await?
                    .iter()
                    .filter_map(|o| {
                        let id = offer_id(o)?;
                        let mut e = json!({"offerId": id});
                        if let Some(pr) = &price {
                            e["price"] = amount(pr, &currency);
                        }
                        if let Some(q) = it.quantity {
                            e["availableQuantity"] = Value::from(q);
                        }
                        Some(e)
                    })
                    .collect();
                if price.is_some() && offers.is_empty() {
                    return Err(EbayError::NotFound(format!(
                        "SKU {sku} has no offer on {} to price",
                        m.id
                    )));
                }
                let mut r = json!({"sku": sku});
                if let Some(q) = it.quantity {
                    r["shipToLocationAvailability"] = json!({"quantity": q});
                }
                if !offers.is_empty() {
                    r["offers"] = Value::from(offers);
                }
                requests.push(r);
            }
            let a = c
                .send(
                    Call::new(
                        Method::POST,
                        "bulk_update_price_quantity",
                        &["sell", "inventory", "v1", "bulk_update_price_quantity"],
                    )
                    .body(json!({"requests": requests})),
                )
                .await?;
            let results: Vec<Value> = a
                .body
                .get("responses")
                .and_then(Value::as_array)
                .map(|rs| {
                    rs.iter()
                        .map(|r| {
                            let status = r.get("statusCode").and_then(Value::as_u64).unwrap_or(0);
                            clean(json!({
                                "sku": r.get("sku"),
                                "offer_id": r.get("offerId"),
                                "ok": (200..300).contains(&status),
                                "errors": r.get("errors").and_then(|e| error_detail(&json!({"errors": e}).to_string())),
                                "warnings": r.get("warnings").and_then(|e| error_detail(&json!({"errors": e}).to_string())),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json_result(&json!({"results": results})))
        })
        .await
    }

    #[tool(
        description = "Publish an unpublished (draft) offer as a live eBay listing, by SKU or offer id. Returns the listing id and link. eBay's error says what is missing when it refuses.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub async fn publish_listing(
        &self,
        Parameters(p): Parameters<PublishParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("publish_listing", async {
            let c = self.caller(&parts)?;
            let m = c.marketplace(p.marketplace.as_deref())?;
            let id = match (p.offer_id.as_deref(), p.sku.as_deref()) {
                (Some(o), None) => id_arg("offer_id", o)?,
                (o, Some(s)) => {
                    let sku = sku_arg(s)?;
                    let offer = the_offer(&c, &sku, m, o).await?;
                    offer_id(&offer)
                        .ok_or_else(|| EbayError::Upstream("an offer without an id".into()))?
                }
                (None, None) => return Err(EbayError::invalid("give sku or offer_id")),
            };
            let (listing, warnings) = publish(&c, &id).await?;
            Ok(json_result(&clean(json!({
                "offer_id": id,
                "listing_id": listing,
                "url": c.listing_url(m, &listing),
                "warnings": warnings,
            }))))
        })
        .await
    }

    #[tool(
        description = "Delete a listing by SKU: removes the inventory item and its offers, and ends any live listing for it straight away. This cannot be undone; to stop selling but keep it for later, use end_listing.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn delete_listing(
        &self,
        Parameters(p): Parameters<SkuParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("delete_listing", async {
            let c = self.caller(&parts)?;
            let sku = sku_arg(&p.sku)?;
            let offers = offers_for(&c, &sku, None).await?;
            c.send(Call::new(
                Method::DELETE,
                "delete_inventory_item",
                &inventory_item_path(&sku),
            ))
            .await
            .map_err(not_in_inventory(&sku))?;
            let ended: Vec<Value> = offers
                .iter()
                .filter_map(|o| o.pointer("/listing/listingId").cloned())
                .collect();
            Ok(json_result(
                &json!({"ok": true, "sku": sku, "ended_listings": ended}),
            ))
        })
        .await
    }

    #[tool(
        description = "Move up to 5 listings made on the eBay website into the inventory model, so the SKU tools (get_listing, update_listing, update_price_quantity) can manage them. Each listing needs a SKU (set it with revise_listing) and the account must use business policies. Returns each listing's SKU and offer id.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn migrate_listings(
        &self,
        Parameters(p): Parameters<MigrateParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("migrate_listings", async {
            let c = self.caller(&parts)?;
            if p.item_ids.is_empty() || p.item_ids.len() > MAX_MIGRATE {
                return Err(EbayError::invalid(format!("give 1 to {MAX_MIGRATE} item_ids")));
            }
            let requests: Vec<Value> = p
                .item_ids
                .iter()
                .map(|i| item_number("item_ids", i).map(|n| json!({"listingId": n})))
                .collect::<error::Result<_>>()?;
            let a = c
                .send(
                    Call::new(
                        Method::POST,
                        "bulk_migrate_listing",
                        &["sell", "inventory", "v1", "bulk_migrate_listing"],
                    )
                    .body(json!({"requests": requests})),
                )
                .await?;
            let results: Vec<Value> = a
                .body
                .get("responses")
                .and_then(Value::as_array)
                .map(|rs| {
                    rs.iter()
                        .map(|r| {
                            let status = r.get("statusCode").and_then(Value::as_u64).unwrap_or(0);
                            clean(json!({
                                "item_id": r.get("listingId"),
                                "marketplace": r.get("marketplaceId"),
                                "ok": (200..300).contains(&status),
                                "skus": r.get("inventoryItems").and_then(Value::as_array).map(|items| {
                                    items.iter().map(|i| json!({"sku": i.get("sku"), "offer_id": i.get("offerId")})).collect::<Vec<_>>()
                                }),
                                "errors": r.get("errors").and_then(|e| error_detail(&json!({"errors": e}).to_string())),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json_result(&json!({"results": results})))
        })
        .await
    }
}

/// A 404 for an inventory item, with a hint for listings made on the website.
fn not_in_inventory(sku: &str) -> impl FnOnce(EbayError) -> EbayError + '_ {
    move |e| match e {
        EbayError::NotFound(_) => EbayError::NotFound(format!(
            "there is no inventory item with SKU {sku}. Listings made on the eBay website are not inventory items: find them with list_active_listings, change them with revise_listing, or move them over with migrate_listings"
        )),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn made_up_skus_are_valid() {
        let s = new_sku();
        assert!(s.starts_with("MCP-"), "{s}");
        assert_eq!(sku_arg(&s).unwrap(), s);
        assert_ne!(new_sku(), new_sku());
    }

    #[test]
    fn read_only_offer_fields_are_dropped() {
        let o = strip(
            json!({"offerId": "1", "sku": "a", "status": "PUBLISHED", "listing": {}, "categoryId": "9", "marketplaceId": "EBAY_GB", "format": "FIXED_PRICE"}),
            OFFER_READ_ONLY,
        );
        assert_eq!(o, json!({"categoryId": "9"}));
    }

    #[test]
    fn best_offer_terms_only_when_asked() {
        assert!(best_offer_terms(None, None, None, "GBP").unwrap().is_none());
        let t = best_offer_terms(None, Some("20"), Some("10"), "GBP")
            .unwrap()
            .unwrap();
        assert_eq!(t["bestOfferEnabled"], true);
        assert_eq!(t["autoAcceptPrice"]["value"], "20.00");
        let off = best_offer_terms(Some(false), Some("20"), None, "GBP")
            .unwrap()
            .unwrap();
        assert_eq!(off, json!({"bestOfferEnabled": false}));
    }

    #[test]
    fn barcodes_are_digits() {
        assert_eq!(
            gtin_list("ean", Some("5702 0159-0001")).unwrap().unwrap(),
            json!(["570201590001"])
        );
        assert!(gtin_list("ean", Some("abc")).is_err());
    }
}
