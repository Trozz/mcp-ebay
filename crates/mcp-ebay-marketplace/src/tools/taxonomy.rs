//! Categories and what they ask of a listing: the category that fits an item, the item specifics
//! (aspects) it requires and the conditions it allows. Public data, read with the application
//! token.

use super::{EbayServer, condition_name, id_arg, json_result, required_text, run};
use crate::api::Call;
use crate::config::Marketplace;
use crate::error::{self, EbayError};
use crate::view::clean;
use axum::http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars::JsonSchema;
use rmcp::{ErrorData as McpError, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

/// Most allowed values shown per item specific.
const MAX_VALUES: usize = 40;

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct SuggestParams {
    /// A few words describing the item, e.g. "lego millennium falcon 75192".
    pub query: String,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RequirementsParams {
    /// The leaf category id, from suggest_categories.
    pub category_id: String,
    /// e.g. EBAY_GB. Default: the server's.
    pub marketplace: Option<String>,
    /// Also list the optional item specifics' names. Default: false.
    pub include_optional: Option<bool>,
}

impl EbayServer {
    /// The category tree id of marketplace `m`, remembered once read.
    async fn tree_id(&self, m: &Marketplace) -> error::Result<String> {
        static TREES: std::sync::Mutex<Vec<(&'static str, String)>> =
            std::sync::Mutex::new(Vec::new());
        if let Some((_, id)) = TREES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(k, _)| *k == m.id)
        {
            return Ok(id.clone());
        }
        let a = self
            .public(
                Call::get(
                    "default_category_tree",
                    &["commerce", "taxonomy", "v1", "get_default_category_tree_id"],
                )
                .query("marketplace_id", m.id),
            )
            .await?;
        let id = a
            .body
            .get("categoryTreeId")
            .and_then(Value::as_str)
            .ok_or_else(|| EbayError::Upstream("eBay named no category tree".into()))?
            .to_string();
        id_arg("categoryTreeId", &id)?;
        TREES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((m.id, id.clone()));
        Ok(id)
    }
}

/// An item specific as tools show it.
fn aspect(a: &Value) -> Value {
    let c = a.get("aspectConstraint");
    let values: Vec<Value> = a
        .get("aspectValues")
        .and_then(Value::as_array)
        .map(|vs| {
            vs.iter()
                .filter_map(|v| v.get("localizedValue").cloned())
                .take(MAX_VALUES)
                .collect()
        })
        .unwrap_or_default();
    let total = a
        .get("aspectValues")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mode = c.and_then(|c| c.get("aspectMode")).and_then(Value::as_str);
    let mut out = json!({
        "name": a.get("localizedAspectName"),
        "free_text": mode != Some("SELECTION_ONLY"),
        "multiple_values": c.and_then(|c| c.get("itemToAspectCardinality")).and_then(Value::as_str) == Some("MULTI"),
        "max_length": c.and_then(|c| c.get("aspectMaxLength")),
        "values": values,
    });
    if total > MAX_VALUES {
        out["more_values"] = Value::from(total - MAX_VALUES);
    }
    clean(out)
}

#[tool_router(router = taxonomy_router, vis = "pub(crate)")]
impl EbayServer {
    #[tool(
        description = "Find the eBay category for an item from a few words. Returns the best matching leaf categories with their id and full path (Toys & Games > Building Toys > LEGO Complete Sets & Packs). Listings must use a leaf category id.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn suggest_categories(
        &self,
        Parameters(p): Parameters<SuggestParams>,
        Extension(_parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("suggest_categories", async {
            let m = self.marketplace(p.marketplace.as_deref())?;
            let q = required_text("query", &p.query)?;
            let tree = self.tree_id(m).await?;
            let a = self
                .public(
                    Call::get(
                        "category_suggestions",
                        &[
                            "commerce",
                            "taxonomy",
                            "v1",
                            "category_tree",
                            &tree,
                            "get_category_suggestions",
                        ],
                    )
                    .query("q", q),
                )
                .await?;
            let items: Vec<Value> = a
                .body
                .get("categorySuggestions")
                .and_then(Value::as_array)
                .map(|ss| {
                    ss.iter()
                        .map(|s| {
                            let mut path: Vec<(i64, String)> = s
                                .get("categoryTreeNodeAncestors")
                                .and_then(Value::as_array)
                                .map(|ancestors| {
                                    ancestors
                                        .iter()
                                        .filter_map(|x| {
                                            Some((
                                                x.get("categoryTreeNodeLevel")?.as_i64()?,
                                                x.get("categoryName")?.as_str()?.to_string(),
                                            ))
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            path.sort_by_key(|(level, _)| *level);
                            let name = s
                                .pointer("/category/categoryName")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let mut names: Vec<String> = path.into_iter().map(|(_, n)| n).collect();
                            names.push(name.to_string());
                            clean(json!({
                                "category_id": s.pointer("/category/categoryId"),
                                "name": name,
                                "path": names.join(" > "),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Ok(json_result(&json!({
                "marketplace": m.id,
                "category_tree_id": tree,
                "count": items.len(),
                "items": items,
            })))
        })
        .await
    }

    #[tool(
        description = "What a category asks of a listing: the required and recommended item specifics (aspects) with their allowed values, and the conditions it allows (with the names create_listing takes). Call this before create_listing so the listing has every required item specific.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    pub async fn get_category_requirements(
        &self,
        Parameters(p): Parameters<RequirementsParams>,
        Extension(_parts): Extension<Parts>,
    ) -> Result<CallToolResult, McpError> {
        run("get_category_requirements", async {
            let m = self.marketplace(p.marketplace.as_deref())?;
            let category = id_arg("category_id", &p.category_id)?;
            let tree = self.tree_id(m).await?;
            let a = self
                .public(
                    Call::get(
                        "item_aspects",
                        &[
                            "commerce",
                            "taxonomy",
                            "v1",
                            "category_tree",
                            &tree,
                            "get_item_aspects_for_category",
                        ],
                    )
                    .query("category_id", category.clone()),
                )
                .await?;
            let all = a
                .body
                .get("aspects")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let usage = |x: &Value| {
                let c = x.get("aspectConstraint");
                if c.and_then(|c| c.get("aspectRequired"))
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    "required"
                } else if c.and_then(|c| c.get("aspectUsage")).and_then(Value::as_str)
                    == Some("RECOMMENDED")
                {
                    "recommended"
                } else {
                    "optional"
                }
            };
            let pick = |u: &str| -> Vec<Value> {
                all.iter().filter(|x| usage(x) == u).map(aspect).collect()
            };
            let mut out = json!({
                "marketplace": m.id,
                "category_id": category,
                "required_aspects": pick("required"),
                "recommended_aspects": pick("recommended"),
            });
            let optional: Vec<Value> = all
                .iter()
                .filter(|x| usage(x) == "optional")
                .filter_map(|x| x.get("localizedAspectName").cloned())
                .collect();
            if p.include_optional.unwrap_or(false) {
                out["optional_aspects"] = Value::from(optional);
            } else {
                out["optional_aspect_count"] = Value::from(optional.len());
            }
            // Conditions come from the Metadata API; a failure there should not hide the aspects.
            match self
                .public(
                    Call::get(
                        "item_condition_policies",
                        &[
                            "sell",
                            "metadata",
                            "v1",
                            "marketplace",
                            m.id,
                            "get_item_condition_policies",
                        ],
                    )
                    .query("filter", format!("categoryIds:{{{category}}}")),
                )
                .await
            {
                Ok(c) => {
                    let policy = c
                        .body
                        .get("itemConditionPolicies")
                        .and_then(Value::as_array)
                        .and_then(|ps| ps.first())
                        .cloned()
                        .unwrap_or(Value::Null);
                    let conditions: Vec<Value> = policy
                        .get("itemConditions")
                        .and_then(Value::as_array)
                        .map(|cs| {
                            cs.iter()
                                .map(|x| {
                                    let id =
                                        x.get("conditionId").and_then(Value::as_str).unwrap_or("");
                                    clean(json!({
                                        "condition": condition_name(id),
                                        "condition_id": id,
                                        "label": x.get("conditionDescription"),
                                    }))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    out["condition_required"] = policy
                        .get("itemConditionRequired")
                        .cloned()
                        .unwrap_or(Value::Null);
                    out["conditions"] = Value::from(conditions);
                }
                Err(e) => {
                    out["conditions_note"] =
                        Value::from(format!("the allowed conditions could not be read: {e}"));
                }
            }
            Ok(json_result(&clean(out)))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aspects_are_summarised_and_long_value_lists_cut() {
        let values: Vec<Value> = (0..50)
            .map(|i| json!({"localizedValue": format!("v{i}")}))
            .collect();
        let a = aspect(&json!({
            "localizedAspectName": "Brand",
            "aspectConstraint": {"aspectRequired": true, "aspectMode": "FREE_TEXT", "itemToAspectCardinality": "SINGLE"},
            "aspectValues": values,
        }));
        assert_eq!(a["name"], "Brand");
        assert_eq!(a["free_text"], true);
        assert_eq!(a["multiple_values"], false);
        assert_eq!(a["values"].as_array().unwrap().len(), MAX_VALUES);
        assert_eq!(a["more_values"], 10);
    }
}
