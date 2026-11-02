//! The seller's account: who signed in, their business policies (shipping, payment, returns) and
//! inventory locations, which every listing made through the Inventory API needs.

use super::{Caller, EbayServer, fields, id_arg, json_result, opt_s, required_text, run, text_arg};
use crate::api::{Call, Host};
use crate::config::Marketplace;
use crate::error::{self, EbayError};
use crate::view::{clean, money};
use axum::http::request::Parts;
use reqwest::Method;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

/// A kind of business policy: its path, the list key in answers and its id field.
#[derive(Clone, Copy)]
pub(crate) struct PolicyKind {
    pub path: &'static str,
    pub list_key: &'static str,
    pub id_key: &'static str,
    /// What a person calls it.
    pub label: &'static str,
    /// The `create_listing` argument that takes it.
    pub arg: &'static str,
}

pub(crate) const FULFILLMENT: PolicyKind = PolicyKind {
    path: "fulfillment_policy",
    list_key: "fulfillmentPolicies",
    id_key: "fulfillmentPolicyId",
    label: "shipping (fulfillment) policy",
    arg: "fulfillment_policy_id",
};
pub(crate) const PAYMENT: PolicyKind = PolicyKind {
    path: "payment_policy",
    list_key: "paymentPolicies",
    id_key: "paymentPolicyId",
    label: "payment policy",
    arg: "payment_policy_id",
};
pub(crate) const RETURN: PolicyKind = PolicyKind {
    path: "return_policy",
    list_key: "returnPolicies",
    id_key: "returnPolicyId",
    label: "return policy",
    arg: "return_policy_id",
};

/// The seller's policies of `kind` on marketplace `m`, as eBay returns them.
pub(crate) async fn policies(
    c: &Caller,
    kind: PolicyKind,
    m: &Marketplace,
) -> error::Result<Vec<Value>> {
    let a = c
        .send(
            Call::get(kind.path, &["sell", "account", "v1", kind.path])
                .query("marketplace_id", m.id),
        )
        .await?;
    Ok(a.body
        .get(kind.list_key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Whether eBay marks policy `p` as the default for ordinary listings.
fn is_default(p: &Value) -> bool {
    p.get("categoryTypes")
        .and_then(Value::as_array)
        .is_some_and(|ts| {
            ts.iter().any(|t| {
                t.get("default").and_then(Value::as_bool) == Some(true)
                    && t.get("name").and_then(Value::as_str) != Some("MOTORS_VEHICLES")
            })
        })
}

/// A policy as tools show it: id, name, whether it is the default, and what it says.
pub(crate) fn policy_summary(kind: PolicyKind, p: &Value) -> Value {
    let mut out = json!({
        "id": p.get(kind.id_key),
        "name": p.get("name"),
        "description": p.get("description"),
        "default": is_default(p),
    });
    let o = out.as_object_mut().expect("an object");
    match kind.path {
        "fulfillment_policy" => {
            if let Some(h) = p.get("handlingTime") {
                o.insert(
                    "handling_time".into(),
                    Value::from(format!(
                        "{} {}",
                        h.get("value").map(Value::to_string).unwrap_or_default(),
                        h.get("unit").and_then(Value::as_str).unwrap_or("DAY")
                    )),
                );
            }
            let services: Vec<Value> = p
                .get("shippingOptions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .flat_map(|opt| {
                    let scope = opt.get("optionType").cloned();
                    opt.get("shippingServices")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .map(move |s| {
                            json!({
                                "scope": scope,
                                "service": s.get("shippingServiceCode"),
                                "cost": money(s.get("shippingCost")),
                                "free": s.get("freeShipping"),
                            })
                        })
                })
                .collect();
            o.insert("shipping_services".into(), Value::from(services));
            o.insert(
                "local_pickup".into(),
                p.get("localPickup").cloned().unwrap_or(Value::Null),
            );
        }
        "return_policy" => {
            o.insert(
                "returns_accepted".into(),
                p.get("returnsAccepted").cloned().unwrap_or(Value::Null),
            );
            if let Some(r) = p.get("returnPeriod") {
                o.insert(
                    "return_period".into(),
                    Value::from(format!(
                        "{} {}",
                        r.get("value").map(Value::to_string).unwrap_or_default(),
                        r.get("unit").and_then(Value::as_str).unwrap_or("DAY")
                    )),
                );
            }
            o.insert(
                "return_shipping_paid_by".into(),
                p.get("returnShippingCostPayer")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
        }
        _ => {
            o.insert(
                "immediate_payment".into(),
                p.get("immediatePay").cloned().unwrap_or(Value::Null),
            );
        }
    }
    clean(out)
}

/// The id of the policy of `kind` to use: `given`, or the only one, or the one eBay marks as the
/// default. Otherwise an error that lists them.
pub(crate) async fn choose_policy(
    c: &Caller,
    kind: PolicyKind,
    m: &Marketplace,
    given: Option<&str>,
) -> error::Result<String> {
    if let Some(g) = given.map(str::trim).filter(|g| !g.is_empty()) {
        return id_arg(kind.arg, g);
    }
    let all = policies(c, kind, m).await?;
    let id = |p: &Value| p.get(kind.id_key).and_then(Value::as_str).map(String::from);
    if all.len() == 1
        && let Some(i) = id(&all[0])
    {
        return Ok(i);
    }
    let defaults: Vec<&Value> = all.iter().filter(|p| is_default(p)).collect();
    if defaults.len() == 1
        && let Some(i) = id(defaults[0])
    {
        return Ok(i);
    }
    if all.is_empty() {
        return Err(EbayError::invalid(format!(
            "this account has no {} on {}. Create one in eBay Seller Hub (Account, Business policies), or opt in to business policies with opt_in_business_policies first; get_seller_setup shows what is missing",
            kind.label, m.id
        )));
    }
    let names: Vec<String> = all
        .iter()
        .map(|p| {
            format!(
                "{} ({})",
                p.get("name").and_then(Value::as_str).unwrap_or("unnamed"),
                id(p).unwrap_or_default()
            )
        })
        .collect();
    Err(EbayError::invalid(format!(
        "this account has several {}s; give {} as one of: {}",
        kind.label,
        kind.arg,
        names.join(", ")
    )))
}

/// The seller's inventory locations, as eBay returns them.
pub(crate) async fn locations(c: &Caller) -> error::Result<Vec<Value>> {
    let a = c
        .send(
            Call::get("locations", &["sell", "inventory", "v1", "location"]).query("limit", "100"),
        )
        .await?;
    Ok(a.body
        .get("locations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn location_summary(l: &Value) -> Value {
    let addr = l.pointer("/location/address");
    clean(json!({
        "key": l.get("merchantLocationKey"),
        "name": l.get("name"),
        "status": l.get("merchantLocationStatus"),
        "types": l.get("locationTypes"),
        "city": addr.and_then(|a| a.get("city")),
        "postal_code": addr.and_then(|a| a.get("postalCode")),
        "country": addr.and_then(|a| a.get("country")),
    }))
}

/// The key of the inventory location to use: `given`, or the only enabled one.
pub(crate) async fn choose_location(c: &Caller, given: Option<&str>) -> error::Result<String> {
    if let Some(g) = given.map(str::trim).filter(|g| !g.is_empty()) {
        return id_arg("merchant_location_key", g);
    }
    let all = locations(c).await?;
    let enabled: Vec<&Value> = all
        .iter()
        .filter(|l| l.get("merchantLocationStatus").and_then(Value::as_str) != Some("DISABLED"))
        .collect();
    let key = |l: &Value| {
        l.get("merchantLocationKey")
            .and_then(Value::as_str)
            .map(String::from)
    };
    match enabled.as_slice() {
        [one] => key(one).ok_or_else(|| EbayError::Upstream("a location without a key".into())),
        [] => Err(EbayError::invalid(
            "this account has no inventory location, which eBay needs to say where items ship from: create one with create_inventory_location",
        )),
        many => Err(EbayError::invalid(format!(
            "this account has several inventory locations; give merchant_location_key as one of: {}",
            many.iter()
                .filter_map(|l| key(l))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SetupParams {
    /// The marketplace whose policies to show, e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CreateLocationParams {
    /// A key of your choosing that names the location in listings, e.g. home or warehouse-1
    /// (letters, digits, - and _; at most 36).
    pub key: String,
    /// A name for it, e.g. "Home".
    pub name: Option<String>,
    pub address_line1: Option<String>,
    pub address_line2: Option<String>,
    pub city: Option<String>,
    /// County, state or province.
    pub state_or_province: Option<String>,
    pub postal_code: Option<String>,
    /// ISO 3166 country code, e.g. GB. Default: the marketplace's.
    pub country: Option<String>,
    pub phone: Option<String>,
}

fn key_arg(v: &str) -> error::Result<String> {
    let t = v.trim();
    if t.is_empty()
        || t.len() > 36
        || !t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(EbayError::invalid(
            "key must be 1 to 36 letters, digits, - or _",
        ));
    }
    Ok(t.to_string())
}

#[tool_router(router = account_router, vis = "pub(crate)")]
impl EbayServer {
    #[tool(
        description = "Start here. The signed-in eBay account (username), whether it uses business policies, its shipping (fulfillment), payment and return policies on the marketplace, its inventory locations, and what is missing before create_listing can list an item (next_steps).",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn get_seller_setup(
        &self,
        Parameters(p): Parameters<SetupParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("get_seller_setup", async {
            let c = self.caller(&parts)?;
            let m = c.marketplace(p.marketplace.as_deref())?;
            let mut notes: Vec<String> = Vec::new();
            let mut next: Vec<String> = Vec::new();
            let account = match c
                .send(
                    Call::get("identity_user", &["commerce", "identity", "v1", "user", ""])
                        .on(Host::Identity),
                )
                .await
            {
                Ok(a) => clean(json!({
                    "username": a.body.get("username"),
                    "user_id": a.body.get("userId"),
                    "account_type": a.body.get("accountType"),
                    "registration_marketplace": a.body.get("registrationMarketplaceId"),
                })),
                Err(e @ EbayError::Auth) => return Err(e),
                Err(e) => {
                    notes.push(format!("account: {e}"));
                    Value::Null
                }
            };
            let opted_in = match c
                .send(Call::get(
                    "opted_in_programs",
                    &["sell", "account", "v1", "program", "get_opted_in_programs"],
                ))
                .await
            {
                Ok(a) => Some(
                    a.body
                        .get("programs")
                        .and_then(Value::as_array)
                        .is_some_and(|ps| {
                            ps.iter().any(|p| {
                                p.get("programType").and_then(Value::as_str)
                                    == Some("SELLING_POLICY_MANAGEMENT")
                            })
                        }),
                ),
                Err(e) => {
                    notes.push(format!("programs: {e}"));
                    None
                }
            };
            if opted_in == Some(false) {
                next.push("Opt in to business policies with opt_in_business_policies (eBay can take a few hours to set them up), then create shipping, payment and return policies in Seller Hub.".into());
            }
            let mut out = json!({
                "marketplace": m.id,
                "account": account,
                "business_policies_opted_in": opted_in,
            });
            let mut ready = opted_in != Some(false);
            for kind in [FULFILLMENT, PAYMENT, RETURN] {
                let key = format!("{}_policies", kind.path.trim_end_matches("_policy"));
                match policies(&c, kind, m).await {
                    Ok(list) => {
                        if list.is_empty() {
                            ready = false;
                            if opted_in != Some(false) {
                                next.push(format!(
                                    "Create a {} for {} in Seller Hub (Account, Business policies).",
                                    kind.label, m.id
                                ));
                            }
                        } else if list.len() > 1 && !list.iter().any(is_default) {
                            next.push(format!(
                                "There are several {}s and none is the default: pass {} to create_listing.",
                                kind.label, kind.arg
                            ));
                        }
                        out[key] = Value::from(
                            list.iter().map(|p| policy_summary(kind, p)).collect::<Vec<_>>(),
                        );
                    }
                    Err(e @ EbayError::Auth) => return Err(e),
                    Err(e) => {
                        ready = false;
                        notes.push(format!("{}: {e}", kind.label));
                    }
                }
            }
            match locations(&c).await {
                Ok(list) => {
                    if list.is_empty() {
                        ready = false;
                        next.push("Create an inventory location (where items ship from) with create_inventory_location.".into());
                    }
                    out["inventory_locations"] =
                        Value::from(list.iter().map(location_summary).collect::<Vec<_>>());
                }
                Err(e) => {
                    ready = false;
                    notes.push(format!("inventory locations: {e}"));
                }
            }
            out["ready_to_list"] = Value::from(ready);
            if !next.is_empty() {
                out["next_steps"] = Value::from(next);
            }
            if !notes.is_empty() {
                out["notes"] = Value::from(notes);
            }
            Ok(json_result(&clean(out)))
        })
        .await
    }

    #[tool(
        description = "Opt the account in to eBay business policies (shipping, payment and return policies), which listings made through this server need. eBay can take a few hours to finish; existing listing settings become policies.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    pub async fn opt_in_business_policies(
        &self,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("opt_in_business_policies", async {
            let c = self.caller(&parts)?;
            let r = c
                .send(
                    Call::new(
                        Method::POST,
                        "opt_in_to_program",
                        &["sell", "account", "v1", "program", "opt_in"],
                    )
                    .body(json!({"programType": "SELLING_POLICY_MANAGEMENT"})),
                )
                .await;
            match r {
                Ok(_) => Ok(json_result(&json!({
                    "ok": true,
                    "note": "Opted in. eBay may take a few hours to set up the policies; check with get_seller_setup."
                }))),
                // eBay answers 409 when the account is already opted in.
                Err(EbayError::Validation(m)) if m.contains("409") || m.to_ascii_lowercase().contains("already") => {
                    Ok(json_result(&json!({"ok": true, "note": "Already opted in."})))
                }
                Err(e) => Err(e),
            }
        })
        .await
    }

    #[tool(
        description = "Create an inventory location: the address items ship from, which eBay needs for every listing made through this server. Give at least the postal code and country, or the city and country.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub async fn create_inventory_location(
        &self,
        Parameters(p): Parameters<CreateLocationParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("create_inventory_location", async {
            let c = self.caller(&parts)?;
            let key = key_arg(&p.key)?;
            let t = |n: &str, v: &Option<String>| -> error::Result<Option<Value>> {
                Ok(opt_s(text_arg(n, v.as_deref())?))
            };
            let country = match text_arg("country", p.country.as_deref())? {
                Some(cc) if cc.len() == 2 && cc.chars().all(|c| c.is_ascii_alphabetic()) => {
                    cc.to_ascii_uppercase()
                }
                Some(_) => {
                    return Err(EbayError::invalid(
                        "country must be a two-letter ISO 3166 code such as GB",
                    ));
                }
                None => c.marketplace(None)?.country.to_string(),
            };
            let address = fields(vec![
                ("addressLine1", t("address_line1", &p.address_line1)?),
                ("addressLine2", t("address_line2", &p.address_line2)?),
                ("city", t("city", &p.city)?),
                (
                    "stateOrProvince",
                    t("state_or_province", &p.state_or_province)?,
                ),
                ("postalCode", t("postal_code", &p.postal_code)?),
                ("country", Some(Value::from(country))),
            ]);
            if !address.contains_key("postalCode") && !address.contains_key("city") {
                return Err(EbayError::invalid(
                    "give postal_code, or city (with state_or_province where the country has them)",
                ));
            }
            let name = match &p.name {
                Some(n) => required_text("name", n)?,
                None => key.clone(),
            };
            let mut body = json!({
                "location": {"address": address},
                "name": name,
                "merchantLocationStatus": "ENABLED",
                "locationTypes": ["WAREHOUSE"],
            });
            if let Some(phone) = text_arg("phone", p.phone.as_deref())? {
                body["phone"] = Value::from(phone);
            }
            c.send(
                Call::new(
                    Method::POST,
                    "create_location",
                    &["sell", "inventory", "v1", "location", &key],
                )
                .body(body),
            )
            .await?;
            Ok(json_result(
                &json!({"ok": true, "merchant_location_key": key}),
            ))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policies_are_summarised() {
        let p = json!({
            "fulfillmentPolicyId": "6001", "name": "Royal Mail", "marketplaceId": "EBAY_GB",
            "categoryTypes": [{"name": "ALL_EXCLUDING_MOTORS_VEHICLES", "default": true}],
            "handlingTime": {"value": 1, "unit": "DAY"},
            "shippingOptions": [{"optionType": "DOMESTIC", "shippingServices": [
                {"shippingServiceCode": "UK_RoyalMailSecondClassStandard", "shippingCost": {"value": "3.20", "currency": "GBP"}}
            ]}]
        });
        let s = policy_summary(FULFILLMENT, &p);
        assert_eq!(s["id"], "6001");
        assert_eq!(s["default"], true);
        assert_eq!(s["handling_time"], "1 DAY");
        assert_eq!(s["shipping_services"][0]["cost"], "3.20 GBP");
        assert!(!is_default(
            &json!({"categoryTypes": [{"name": "MOTORS_VEHICLES", "default": true}]})
        ));
    }

    #[test]
    fn location_keys_are_checked() {
        assert_eq!(key_arg(" home ").unwrap(), "home");
        assert!(key_arg("my home").is_err());
        assert!(key_arg(&"k".repeat(37)).is_err());
    }
}
