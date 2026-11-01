//! The account's orders as a seller, through the Fulfillment API: what has sold, where it goes,
//! and marking it posted with tracking.

use super::{EbayServer, choice, id_arg, json_result, paging, required_text, run, timestamp_arg};
use crate::api::Call;
use crate::error::{self, EbayError};
use crate::view::{clean, money, page};
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
pub struct ListOrdersParams {
    /// awaiting_shipment (default: not yet or partly posted), shipped, or all.
    pub status: Option<String>,
    /// Only orders placed on or after, as YYYY-MM-DD or an ISO 8601 date-time. eBay keeps orders for 2 years.
    pub created_from: Option<String>,
    /// Only orders placed before, as YYYY-MM-DD or an ISO 8601 date-time.
    pub created_to: Option<String>,
    /// 1 to 200. Default: 25.
    pub limit: Option<u32>,
    /// Orders to skip: pass next_offset for the next page.
    pub offset: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct OrderParams {
    /// The order id, e.g. 12-34567-89012.
    pub order_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ShipLine {
    /// The line item id, from get_order.
    pub line_item_id: String,
    /// How many of it were posted. Default: all of it.
    pub quantity: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ShipParams {
    pub order_id: String,
    /// The carrier, e.g. Royal Mail, Evri, DPD, UPS, USPS.
    pub carrier: String,
    pub tracking_number: String,
    /// The line items posted in this parcel. Default: every line item of the order, in full.
    pub line_items: Option<Vec<ShipLine>>,
    /// When it was posted, as YYYY-MM-DD or an ISO 8601 date-time. Default: now.
    pub shipped_date: Option<String>,
}

/// Where an order goes.
fn ship_to(o: &Value, full: bool) -> Value {
    let s = o
        .get("fulfillmentStartInstructions")
        .and_then(Value::as_array)
        .and_then(|f| f.first())
        .and_then(|f| f.pointer("/shippingStep/shipTo"));
    let Some(s) = s else {
        return Value::Null;
    };
    let a = s.get("contactAddress");
    let g = |k: &str| a.and_then(|a| a.get(k)).cloned().unwrap_or(Value::Null);
    if full {
        clean(json!({
            "name": s.get("fullName"),
            "address_line1": g("addressLine1"),
            "address_line2": g("addressLine2"),
            "city": g("city"),
            "county": g("stateOrProvince"),
            "postal_code": g("postalCode"),
            "country": g("countryCode"),
            "phone": s.pointer("/primaryPhone/phoneNumber"),
            "email": s.get("email"),
        }))
    } else {
        clean(json!({
            "name": s.get("fullName"),
            "city": g("city"),
            "postal_code": g("postalCode"),
            "country": g("countryCode"),
        }))
    }
}

fn line(l: &Value) -> Value {
    clean(json!({
        "line_item_id": l.get("lineItemId"),
        "item_id": l.get("legacyItemId"),
        "sku": l.get("sku"),
        "title": l.get("title"),
        "quantity": l.get("quantity"),
        "price": money(l.get("lineItemCost")),
        "status": l.get("lineItemFulfillmentStatus"),
    }))
}

fn order(o: &Value, full: bool) -> Value {
    let lines: Vec<Value> = o
        .get("lineItems")
        .and_then(Value::as_array)
        .map(|ls| ls.iter().map(line).collect())
        .unwrap_or_default();
    let instructions = o
        .get("fulfillmentStartInstructions")
        .and_then(Value::as_array)
        .and_then(|f| f.first());
    let mut out = json!({
        "order_id": o.get("orderId"),
        "created": o.get("creationDate"),
        "status": o.get("orderFulfillmentStatus"),
        "payment_status": o.get("orderPaymentStatus"),
        "cancel_status": o.pointer("/cancelStatus/cancelState").filter(|s| s.as_str() != Some("NONE_REQUESTED")),
        "buyer": o.pointer("/buyer/username"),
        "total": money(o.pointer("/pricingSummary/total")),
        "items": lines,
        "ship_to": ship_to(o, full),
        "shipping_service": instructions.and_then(|i| i.pointer("/shippingStep/shippingServiceCode")),
        "ship_by": instructions.and_then(|i| i.get("maxEstimatedDeliveryDate")),
    });
    if full {
        out["subtotal"] = Value::from(money(o.pointer("/pricingSummary/priceSubtotal")));
        out["postage"] = Value::from(money(o.pointer("/pricingSummary/deliveryCost")));
        out["buyer_note"] = o.get("buyerCheckoutNotes").cloned().unwrap_or(Value::Null);
        out["fulfilments"] = o.get("fulfillmentHrefs").cloned().unwrap_or(Value::Null);
    }
    clean(out)
}

fn tracking_arg(v: &str) -> error::Result<String> {
    let t = required_text("tracking_number", v)?;
    if t.len() > 60 || !t.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(EbayError::invalid(
            "tracking_number must be letters and digits only (no spaces or dashes), at most 60",
        ));
    }
    Ok(t)
}

#[tool_router(router = orders_router, vis = "pub(crate)")]
impl EbayServer {
    #[tool(
        description = "The account's orders as a seller, newest first: by default those awaiting shipment. Each has its order id, date, buyer, total, items (title, SKU, quantity), where it goes (name, town, postcode) and status.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn list_orders(
        &self,
        Parameters(p): Parameters<ListOrdersParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("list_orders", async {
            let c = self.caller(&parts)?;
            let (limit, offset) = paging(p.limit, p.offset, 25, 200)?;
            let mut filters = Vec::new();
            match choice(
                "status",
                p.status.as_deref(),
                &["awaiting_shipment", "shipped", "all"],
            )? {
                None | Some("awaiting_shipment") => {
                    filters.push("orderfulfillmentstatus:{NOT_STARTED|IN_PROGRESS}".to_string())
                }
                Some("shipped") => filters.push("orderfulfillmentstatus:{FULFILLED}".to_string()),
                _ => {}
            }
            let from = timestamp_arg("created_from", p.created_from.as_deref())?;
            let to = timestamp_arg("created_to", p.created_to.as_deref())?;
            if from.is_some() || to.is_some() {
                filters.push(format!(
                    "creationdate:[{}..{}]",
                    from.unwrap_or_default(),
                    to.unwrap_or_default()
                ));
            }
            let mut call = Call::get("orders", &["sell", "fulfillment", "v1", "order"])
                .query("limit", limit.to_string())
                .query("offset", offset.to_string());
            if !filters.is_empty() {
                call = call.query("filter", filters.join(","));
            }
            let a = c.send(call).await?;
            let items: Vec<Value> = a
                .body
                .get("orders")
                .and_then(Value::as_array)
                .map(|os| os.iter().map(|o| order(o, false)).collect())
                .unwrap_or_default();
            Ok(json_result(&page(
                items,
                offset,
                a.body.get("total").and_then(Value::as_u64),
            )))
        })
        .await
    }

    #[tool(
        description = "One order in full: items with their line item ids, the buyer, the full delivery address, postage service, totals, the buyer's note and status.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn get_order(
        &self,
        Parameters(p): Parameters<OrderParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("get_order", async {
            let c = self.caller(&parts)?;
            let id = id_arg("order_id", &p.order_id)?;
            let a = c
                .send(Call::get(
                    "order",
                    &["sell", "fulfillment", "v1", "order", &id],
                ))
                .await?;
            Ok(json_result(&order(&a.body, true)))
        })
        .await
    }

    #[tool(
        description = "Mark an order (or some of its items) as posted, with the carrier and tracking number. eBay tells the buyer. Defaults to every item of the order.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub async fn mark_order_shipped(
        &self,
        Parameters(p): Parameters<ShipParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("mark_order_shipped", async {
            let c = self.caller(&parts)?;
            let id = id_arg("order_id", &p.order_id)?;
            let carrier = required_text("carrier", &p.carrier)?;
            if carrier.len() > 60 {
                return Err(EbayError::invalid("carrier is limited to 60 characters"));
            }
            let tracking = tracking_arg(&p.tracking_number)?;
            let shipped = timestamp_arg("shipped_date", p.shipped_date.as_deref())?.unwrap_or_else(|| {
                chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
            });
            let lines: Vec<Value> = match p.line_items.as_deref().filter(|l| !l.is_empty()) {
                Some(ls) => {
                    let need_order = ls.iter().any(|l| l.quantity.is_none());
                    let o = if need_order {
                        Some(c.send(Call::get("order", &["sell", "fulfillment", "v1", "order", &id])).await?.body)
                    } else {
                        None
                    };
                    ls.iter()
                        .map(|l| {
                            let lid = id_arg("line_item_id", &l.line_item_id)?;
                            let q = match l.quantity {
                                Some(q) if q > 0 => q as u64,
                                Some(_) => return Err(EbayError::invalid("quantity must be at least 1")),
                                None => o
                                    .as_ref()
                                    .and_then(|o| o.get("lineItems"))
                                    .and_then(Value::as_array)
                                    .and_then(|ls| ls.iter().find(|x| x.get("lineItemId").and_then(Value::as_str) == Some(lid.as_str())))
                                    .and_then(|x| x.get("quantity"))
                                    .and_then(Value::as_u64)
                                    .ok_or_else(|| EbayError::NotFound(format!("order {id} has no line item {lid}")))?,
                            };
                            Ok(json!({"lineItemId": lid, "quantity": q}))
                        })
                        .collect::<error::Result<_>>()?
                }
                None => {
                    let o = c.send(Call::get("order", &["sell", "fulfillment", "v1", "order", &id])).await?;
                    o.body
                        .get("lineItems")
                        .and_then(Value::as_array)
                        .map(|ls| {
                            ls.iter()
                                .filter_map(|l| {
                                    Some(json!({"lineItemId": l.get("lineItemId")?, "quantity": l.get("quantity")?}))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                }
            };
            if lines.is_empty() {
                return Err(EbayError::invalid(format!("order {id} has no line items to ship")));
            }
            let a = c
                .send(
                    Call::new(
                        Method::POST,
                        "create_shipping_fulfillment",
                        &["sell", "fulfillment", "v1", "order", &id, "shipping_fulfillment"],
                    )
                    .body(json!({
                        "lineItems": lines,
                        "shippedDate": shipped,
                        "shippingCarrierCode": carrier,
                        "trackingNumber": tracking,
                    })),
                )
                .await?;
            let fulfillment_id = a
                .location
                .as_deref()
                .and_then(|l| l.trim_end_matches('/').rsplit('/').next())
                .map(String::from);
            Ok(json_result(&clean(json!({
                "ok": true,
                "order_id": id,
                "fulfillment_id": fulfillment_id,
                "line_items": lines,
                "carrier": carrier,
                "tracking_number": tracking,
                "shipped": shipped,
            }))))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_are_summarised_without_the_full_address() {
        let o = json!({
            "orderId": "12-34567-89012", "creationDate": "2026-10-30T10:00:00.000Z",
            "orderFulfillmentStatus": "NOT_STARTED", "orderPaymentStatus": "PAID",
            "buyer": {"username": "buyer1"},
            "pricingSummary": {"total": {"value": "23.20", "currency": "GBP"}},
            "cancelStatus": {"cancelState": "NONE_REQUESTED"},
            "lineItems": [{"lineItemId": "1001", "legacyItemId": "123456789012", "sku": "L-1", "title": "Lamp", "quantity": 1, "lineItemCost": {"value": "20.00", "currency": "GBP"}}],
            "fulfillmentStartInstructions": [{"shippingStep": {"shippingServiceCode": "UK_RoyalMailSecondClassStandard", "shipTo": {"fullName": "Ann Lee", "contactAddress": {"addressLine1": "1 High St", "city": "York", "postalCode": "YO1 1AA", "countryCode": "GB"}}}}]
        });
        let s = order(&o, false);
        assert_eq!(s["total"], "23.20 GBP");
        assert_eq!(s["ship_to"]["city"], "York");
        assert!(s["ship_to"].get("address_line1").is_none());
        assert!(s.get("cancel_status").is_none());
        let f = order(&o, true);
        assert_eq!(f["ship_to"]["address_line1"], "1 High St");
    }

    #[test]
    fn tracking_numbers_are_plain() {
        assert_eq!(tracking_arg(" AB123456789GB ").unwrap(), "AB123456789GB");
        assert!(tracking_arg("AB 123").is_err());
    }
}
