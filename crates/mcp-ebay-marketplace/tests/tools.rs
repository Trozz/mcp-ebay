//! The tools against the fake eBay: what they send, the defaults they fill in, what they refuse,
//! and how they shape answers.
mod support;

use serde_json::{Value, json};
use support::fake_ebay::{Canned, FakeEbay};
use support::tools::{auth, auth_with, error_text, json, no_auth, params, server};

#[tokio::test]
async fn tools_need_a_sign_in() {
    let fake = FakeEbay::start().await;
    let s = server(&fake);
    let r = s
        .get_seller_setup(params(json!({})), no_auth())
        .await
        .unwrap();
    assert!(error_text(&r).starts_with("auth: "));
}

#[tokio::test]
async fn the_refresh_token_becomes_an_access_token_once() {
    let fake = FakeEbay::start().await;
    let s = server(&fake);
    let v = json(&s.get_seller_setup(params(json!({})), auth()).await.unwrap());
    assert_eq!(v["account"]["username"], "ann_sells");
    assert_eq!(v["ready_to_list"], true);
    assert_eq!(v["fulfillment_policies"][0]["id"], "F1");
    assert_eq!(v["inventory_locations"][0]["key"], "home");
    assert!(v.get("next_steps").is_none(), "{v}");
    // One refresh served every call, with the scopes asked for at sign-in.
    let st = fake.lock();
    assert_eq!(st.refreshes, 1);
    let refresh = st
        .log
        .iter()
        .find(|l| l.form("grant_type") == Some("refresh_token"))
        .unwrap();
    assert!(refresh.form("scope").unwrap().contains("sell.inventory"));
    drop(st);
    let id = fake.last("GET", "commerce/identity/v1/user/");
    assert_eq!(id.header("authorization"), Some("Bearer user-1"));
}

#[tokio::test]
async fn a_revoked_sign_in_says_to_sign_in_again() {
    let fake = FakeEbay::start().await;
    let s = server(&fake);
    let r = s
        .get_seller_setup(params(json!({})), auth_with("v^1.1#revoked"))
        .await
        .unwrap();
    assert!(error_text(&r).contains("Sign in to eBay again"));
}

#[tokio::test]
async fn an_access_token_refused_early_is_refreshed_and_the_call_sent_again() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/fulfillment/v1/order",
        Canned::ok(json!({"total": 0, "orders": []})),
    );
    let s = server(&fake);
    json(&s.list_orders(params(json!({})), auth()).await.unwrap());
    // eBay forgets the token.
    fake.lock().user_tokens.clear();
    let v = json(&s.list_orders(params(json!({})), auth()).await.unwrap());
    assert_eq!(v["count"], 0);
    assert_eq!(fake.lock().refreshes, 2);
}

#[tokio::test]
async fn setup_says_what_is_missing() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/account/v1/program/get_opted_in_programs",
        Canned::ok(json!({"programs": []})),
    );
    fake.on(
        "GET",
        "sell/inventory/v1/location",
        Canned::ok(json!({"total": 0, "locations": []})),
    );
    let s = server(&fake);
    let v = json(&s.get_seller_setup(params(json!({})), auth()).await.unwrap());
    assert_eq!(v["ready_to_list"], false);
    assert_eq!(v["business_policies_opted_in"], false);
    let steps = v["next_steps"].to_string();
    assert!(steps.contains("opt_in_business_policies"), "{steps}");
    assert!(steps.contains("create_inventory_location"), "{steps}");
}

fn listing_args() -> Value {
    json!({
        "title": "LEGO Star Wars 75192 Millennium Falcon UCS Complete",
        "description": "<p>Complete with box & instructions.</p>",
        "category_id": "183448",
        "condition": "used excellent",
        "condition_description": "Box worn at corners",
        "price": "£549.9",
        "quantity": 1,
        "aspects": {"Brand": "LEGO", "LEGO Set Number": 75192, "Theme": ["Star Wars"]},
        "image_urls": ["https://img.example.com/falcon1.jpg"],
        "sku": "FALCON-1",
        "best_offer": true,
        "best_offer_auto_decline": "450"
    })
}

#[tokio::test]
async fn create_listing_creates_the_item_and_offer_and_publishes() {
    let fake = FakeEbay::start().await;
    fake.on(
        "POST",
        "sell/inventory/v1/offer",
        Canned::status(201, json!({"offerId": "OF-9"})),
    );
    fake.on(
        "POST",
        "sell/inventory/v1/offer/OF-9/publish",
        Canned::ok(json!({"listingId": "110551234567"})),
    );
    let s = server(&fake);
    let v = json(
        &s.create_listing(params(listing_args()), auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["sku"], "FALCON-1");
    assert_eq!(v["offer_id"], "OF-9");
    assert_eq!(v["listing_id"], "110551234567");
    assert_eq!(v["status"], "PUBLISHED");
    assert_eq!(v["url"], "https://www.ebay.co.uk/itm/110551234567");

    let item = fake.last("PUT", "sell/inventory/v1/inventory_item/FALCON-1");
    assert_eq!(item.header("content-language"), Some("en-GB"));
    let b = item.body.unwrap();
    assert_eq!(b["condition"], "USED_EXCELLENT");
    assert_eq!(b["conditionDescription"], "Box worn at corners");
    assert_eq!(
        b["availability"]["shipToLocationAvailability"]["quantity"],
        1
    );
    assert_eq!(b["product"]["aspects"]["LEGO Set Number"], json!(["75192"]));
    assert_eq!(
        b["product"]["imageUrls"][0],
        "https://img.example.com/falcon1.jpg"
    );

    let offer = fake.last("POST", "sell/inventory/v1/offer").body.unwrap();
    assert_eq!(offer["sku"], "FALCON-1");
    assert_eq!(offer["marketplaceId"], "EBAY_GB");
    assert_eq!(offer["format"], "FIXED_PRICE");
    assert_eq!(offer["listingDuration"], "GTC");
    assert_eq!(offer["categoryId"], "183448");
    assert_eq!(offer["merchantLocationKey"], "home");
    assert_eq!(
        offer["pricingSummary"]["price"],
        json!({"value": "549.90", "currency": "GBP"})
    );
    assert_eq!(offer["listingPolicies"]["fulfillmentPolicyId"], "F1");
    assert_eq!(offer["listingPolicies"]["paymentPolicyId"], "P1");
    assert_eq!(offer["listingPolicies"]["returnPolicyId"], "R1");
    assert_eq!(
        offer["listingPolicies"]["bestOfferTerms"]["autoDeclinePrice"]["value"],
        "450.00"
    );
}

#[tokio::test]
async fn a_new_sku_answered_with_offer_not_available_is_still_listed() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/inventory/v1/offer",
        Canned::status(
            400,
            json!({"errors": [{"errorId": 25713, "message": "This Offer is not available."}]}),
        ),
    );
    fake.on(
        "POST",
        "sell/inventory/v1/offer",
        Canned::status(201, json!({"offerId": "OF-9"})),
    );
    fake.on(
        "POST",
        "sell/inventory/v1/offer/OF-9/publish",
        Canned::ok(json!({"listingId": "1"})),
    );
    let s = server(&fake);
    let v = json(
        &s.create_listing(params(listing_args()), auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["status"], "PUBLISHED");
}

#[tokio::test]
async fn an_auction_has_a_start_price_and_a_duration() {
    let fake = FakeEbay::start().await;
    fake.on(
        "POST",
        "sell/inventory/v1/offer",
        Canned::status(201, json!({"offerId": "OF-1"})),
    );
    let s = server(&fake);
    let mut a = listing_args();
    a["format"] = json!("auction");
    a["auction_days"] = json!(5);
    a["reserve_price"] = json!("400");
    a["publish"] = json!(false);
    a.as_object_mut().unwrap().remove("best_offer");
    a.as_object_mut().unwrap().remove("best_offer_auto_decline");
    let v = json(&s.create_listing(params(a), auth()).await.unwrap());
    assert_eq!(v["status"], "UNPUBLISHED");
    let offer = fake.last("POST", "sell/inventory/v1/offer").body.unwrap();
    assert_eq!(offer["format"], "AUCTION");
    assert_eq!(offer["listingDuration"], "DAYS_5");
    assert_eq!(
        offer["pricingSummary"]["auctionStartPrice"]["value"],
        "549.90"
    );
    assert_eq!(
        offer["pricingSummary"]["auctionReservePrice"]["value"],
        "400.00"
    );
    assert!(
        fake.calls("POST", "sell/inventory/v1/offer/OF-1/publish")
            .is_empty()
    );
}

#[tokio::test]
async fn a_refused_publish_keeps_the_draft_and_says_how_to_fix_it() {
    let fake = FakeEbay::start().await;
    fake.on(
        "POST",
        "sell/inventory/v1/offer",
        Canned::status(201, json!({"offerId": "OF-9"})),
    );
    fake.on(
        "POST",
        "sell/inventory/v1/offer/OF-9/publish",
        Canned::status(400, json!({"errors": [{"errorId": 25002, "message": "A user error has occurred. The item specific Type is missing.", "parameters": [{"name": "0", "value": "Type"}]}]})),
    );
    let s = server(&fake);
    let e = error_text(
        &s.create_listing(params(listing_args()), auth())
            .await
            .unwrap(),
    );
    assert!(e.starts_with("validation: "), "{e}");
    assert!(e.contains("sku FALCON-1, offer OF-9"), "{e}");
    assert!(e.contains("Type is missing"), "{e}");
    assert!(e.contains("publish_listing"), "{e}");
}

#[tokio::test]
async fn an_sku_already_listed_is_not_listed_twice() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/inventory/v1/offer",
        Canned::ok(json!({"offers": [{"offerId": "OF-1", "sku": "FALCON-1", "marketplaceId": "EBAY_GB", "format": "FIXED_PRICE", "status": "PUBLISHED"}]})),
    );
    let s = server(&fake);
    let e = error_text(
        &s.create_listing(params(listing_args()), auth())
            .await
            .unwrap(),
    );
    assert!(
        e.contains("already listed") && e.contains("update_listing"),
        "{e}"
    );
    assert!(
        fake.calls("PUT", "sell/inventory/v1/inventory_item/FALCON-1")
            .is_empty()
    );
}

#[tokio::test]
async fn several_policies_without_a_default_must_be_chosen() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/account/v1/fulfillment_policy",
        Canned::ok(json!({"fulfillmentPolicies": [
            {"fulfillmentPolicyId": "F1", "name": "Royal Mail", "categoryTypes": [{"name": "ALL_EXCLUDING_MOTORS_VEHICLES"}]},
            {"fulfillmentPolicyId": "F2", "name": "Courier", "categoryTypes": [{"name": "ALL_EXCLUDING_MOTORS_VEHICLES"}]}
        ]})),
    );
    let s = server(&fake);
    let e = error_text(
        &s.create_listing(params(listing_args()), auth())
            .await
            .unwrap(),
    );
    assert!(
        e.contains("fulfillment_policy_id") && e.contains("Courier (F2)"),
        "{e}"
    );
    let mut a = listing_args();
    a["fulfillment_policy_id"] = json!("F2");
    fake.on(
        "POST",
        "sell/inventory/v1/offer",
        Canned::status(201, json!({"offerId": "OF-2"})),
    );
    fake.on(
        "POST",
        "sell/inventory/v1/offer/OF-2/publish",
        Canned::ok(json!({"listingId": "1"})),
    );
    json(&s.create_listing(params(a), auth()).await.unwrap());
    let offer = fake.last("POST", "sell/inventory/v1/offer").body.unwrap();
    assert_eq!(offer["listingPolicies"]["fulfillmentPolicyId"], "F2");
}

#[tokio::test]
async fn create_listing_refuses_bad_input_before_calling_ebay() {
    let fake = FakeEbay::start().await;
    let s = server(&fake);
    for (k, v, want) in [
        ("title", json!("x".repeat(81)), "title is limited to 80"),
        ("condition", json!("mint"), "condition must be one of"),
        ("price", json!("-5"), "price must be an amount"),
        (
            "image_urls",
            json!(["http://insecure.example/a.jpg"]),
            "https",
        ),
        ("sku", json!("has space"), "sku must be"),
    ] {
        let mut a = listing_args();
        a[k] = v;
        let e = error_text(&s.create_listing(params(a), auth()).await.unwrap());
        assert!(
            e.starts_with("validation: ") && e.contains(want),
            "{k}: {e}"
        );
    }
    assert!(fake.api_log().is_empty(), "{:?}", fake.api_log());
}

fn existing_item() -> Value {
    json!({
        "sku": "LAMP-1", "locale": "en_GB",
        "product": {"title": "Old lamp", "description": "<p>Old</p>", "aspects": {"Brand": ["Anglepoise"], "Colour": ["Black"]}, "imageUrls": ["https://img.example.com/1.jpg"]},
        "condition": "USED_GOOD",
        "availability": {"shipToLocationAvailability": {"quantity": 2}}
    })
}

fn existing_offer() -> Value {
    json!({"offers": [{
        "offerId": "OF-5", "sku": "LAMP-1", "marketplaceId": "EBAY_GB", "format": "FIXED_PRICE",
        "status": "PUBLISHED", "availableQuantity": 2, "categoryId": "112581",
        "listingDescription": "<p>Old</p>",
        "pricingSummary": {"price": {"value": "40.00", "currency": "GBP"}},
        "listingPolicies": {"fulfillmentPolicyId": "F1", "paymentPolicyId": "P1", "returnPolicyId": "R1"},
        "listing": {"listingId": "110559999999", "listingStatus": "ACTIVE", "soldQuantity": 1}
    }]})
}

#[tokio::test]
async fn update_listing_changes_only_what_is_given_and_updates_the_live_listing() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/inventory/v1/inventory_item/LAMP-1",
        Canned::ok(existing_item()),
    );
    fake.on(
        "GET",
        "sell/inventory/v1/offer",
        Canned::ok(existing_offer()),
    );
    let s = server(&fake);
    let v = json(
        &s.update_listing(
            params(json!({
                "sku": "LAMP-1",
                "title": "Anglepoise 1227 desk lamp, black",
                "description": "<p>New</p>",
                "aspects": {"colour": "Matt Black", "Model": "1227"},
                "remove_aspects": ["brand"],
                "quantity": 3,
                "price": "35"
            })),
            auth(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(v["published"], true);
    assert_eq!(v["url"], "https://www.ebay.co.uk/itm/110559999999");
    let changed = v["changed"].to_string();
    for c in [
        "title",
        "description",
        "item specifics",
        "quantity",
        "price",
    ] {
        assert!(changed.contains(c), "{c}: {changed}");
    }
    let item = fake
        .last("PUT", "sell/inventory/v1/inventory_item/LAMP-1")
        .body
        .unwrap();
    assert!(item.get("sku").is_none() && item.get("locale").is_none());
    assert_eq!(item["product"]["title"], "Anglepoise 1227 desk lamp, black");
    assert_eq!(
        item["product"]["aspects"],
        json!({"colour": ["Matt Black"], "Model": ["1227"]})
    );
    assert_eq!(
        item["product"]["imageUrls"][0],
        "https://img.example.com/1.jpg"
    );
    assert_eq!(item["condition"], "USED_GOOD");
    assert_eq!(
        item["availability"]["shipToLocationAvailability"]["quantity"],
        3
    );
    let offer = fake.last("PUT", "sell/inventory/v1/offer/OF-5");
    assert_eq!(offer.header("content-language"), Some("en-GB"));
    let o = offer.body.unwrap();
    for k in [
        "offerId",
        "sku",
        "marketplaceId",
        "format",
        "status",
        "listing",
    ] {
        assert!(o.get(k).is_none(), "{k} was sent back");
    }
    assert_eq!(
        o["pricingSummary"]["price"],
        json!({"value": "35.00", "currency": "GBP"})
    );
    assert_eq!(o["availableQuantity"], 3);
    assert_eq!(o["listingDescription"], "<p>New</p>");
    assert_eq!(o["listingPolicies"]["returnPolicyId"], "R1");
}

#[tokio::test]
async fn updating_a_website_listing_by_sku_points_to_revise_listing() {
    let fake = FakeEbay::start().await;
    let s = server(&fake);
    let e = error_text(
        &s.update_listing(params(json!({"sku": "NOPE", "title": "x"})), auth())
            .await
            .unwrap(),
    );
    assert!(
        e.starts_with("not_found: ") && e.contains("revise_listing"),
        "{e}"
    );
}

#[tokio::test]
async fn price_and_quantity_change_in_bulk() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/inventory/v1/offer",
        Canned::ok(existing_offer()),
    );
    fake.on(
        "POST",
        "sell/inventory/v1/bulk_update_price_quantity",
        Canned::ok(json!({"responses": [{"statusCode": 200, "sku": "LAMP-1", "offerId": "OF-5"}]})),
    );
    let s = server(&fake);
    let v = json(
        &s.update_price_quantity(
            params(json!({"items": [{"sku": "LAMP-1", "price": "30", "quantity": 4}]})),
            auth(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(v["results"][0]["ok"], true);
    let b = fake
        .last("POST", "sell/inventory/v1/bulk_update_price_quantity")
        .body
        .unwrap();
    assert_eq!(
        b,
        json!({"requests": [{"sku": "LAMP-1", "shipToLocationAvailability": {"quantity": 4}, "offers": [{"offerId": "OF-5", "price": {"value": "30.00", "currency": "GBP"}, "availableQuantity": 4}]}]})
    );
}

#[tokio::test]
async fn end_listing_withdraws_by_sku_or_ends_by_item_number() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/inventory/v1/offer",
        Canned::ok(existing_offer()),
    );
    fake.on(
        "POST",
        "sell/inventory/v1/offer/OF-5/withdraw",
        Canned::ok(json!({"listingId": "110559999999"})),
    );
    let s = server(&fake);
    let v = json(
        &s.end_listing(params(json!({"sku": "LAMP-1"})), auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["withdrawn"][0]["listing_id"], "110559999999");

    fake.on_trading(
        "EndItem",
        r#"<?xml version="1.0"?><EndItemResponse xmlns="urn:ebay:apis:eBLBaseComponents"><Ack>Success</Ack><EndTime>2026-11-01T12:00:00.000Z</EndTime></EndItemResponse>"#,
    );
    let v = json(
        &s.end_listing(
            params(json!({"item_id": "https://www.ebay.co.uk/itm/123456789012", "reason": "lost or broken"})),
            auth(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(v["ended"], "2026-11-01T12:00:00.000Z");
    let call = fake.last_trading("EndItem");
    assert_eq!(call.header("x-ebay-api-siteid"), Some("3"));
    assert!(
        call.header("x-ebay-api-iaf-token")
            .unwrap()
            .starts_with("user-")
    );
    assert!(
        call.raw_body
            .contains("<ItemID>123456789012</ItemID><EndingReason>LostOrBroken</EndingReason>"),
        "{}",
        call.raw_body
    );

    let e = error_text(
        &s.end_listing(
            params(json!({"sku": "A", "item_id": "123456789012"})),
            auth(),
        )
        .await
        .unwrap(),
    );
    assert!(e.contains("exactly one"), "{e}");
}

#[tokio::test]
async fn trading_failures_are_reported_with_ebays_reason() {
    let fake = FakeEbay::start().await;
    fake.on_trading(
        "EndItem",
        r#"<?xml version="1.0"?><EndItemResponse xmlns="urn:ebay:apis:eBLBaseComponents"><Ack>Failure</Ack><Errors><ShortMessage>Auction ended.</ShortMessage><LongMessage>The auction has already been closed.</LongMessage><ErrorCode>1047</ErrorCode><SeverityCode>Error</SeverityCode></Errors></EndItemResponse>"#,
    );
    let s = server(&fake);
    let e = error_text(
        &s.end_listing(params(json!({"item_id": "123456789012"})), auth())
            .await
            .unwrap(),
    );
    assert!(
        e.starts_with("validation: ") && e.contains("already been closed. [error 1047]"),
        "{e}"
    );
}

#[tokio::test]
async fn delete_listing_removes_the_item_and_says_which_listings_ended() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/inventory/v1/offer",
        Canned::ok(existing_offer()),
    );
    let s = server(&fake);
    let v = json(
        &s.delete_listing(params(json!({"sku": "LAMP-1"})), auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["ended_listings"], json!(["110559999999"]));
    fake.last("DELETE", "sell/inventory/v1/inventory_item/LAMP-1");
}

#[tokio::test]
async fn list_and_get_inventory_items() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/inventory/v1/inventory_item",
        Canned::ok(json!({"total": 30, "size": 1, "inventoryItems": [existing_item()]})),
    );
    fake.on(
        "GET",
        "sell/inventory/v1/inventory_item/LAMP-1",
        Canned::ok(existing_item()),
    );
    fake.on(
        "GET",
        "sell/inventory/v1/offer",
        Canned::ok(existing_offer()),
    );
    let s = server(&fake);
    let v = json(
        &s.list_inventory_items(
            params(json!({"page": 2, "per_page": 10, "include_offers": true})),
            auth(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(v["items"][0]["title"], "Old lamp");
    assert_eq!(v["items"][0]["offers"][0]["price"], "40.00 GBP");
    assert_eq!(v["next_page"], 3);
    let l = fake.last("GET", "sell/inventory/v1/inventory_item");
    assert_eq!(l.param("offset"), Some("1"));
    assert_eq!(l.param("limit"), Some("10"));
    let v = json(
        &s.get_listing(params(json!({"sku": "LAMP-1"})), auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["description_text"], "Old");
    assert_eq!(v["offers"][0]["listing_status"], "ACTIVE");
    assert_eq!(v["offers"][0]["sold_quantity"], 1);
}

#[tokio::test]
async fn search_uses_the_app_token_filters_and_marketplace() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "buy/browse/v1/item_summary/search",
        Canned::ok(json!({"total": 45, "itemSummaries": [{
            "itemId": "v1|110551111111|0", "legacyItemId": "110551111111", "title": "LEGO 75192",
            "price": {"value": "500.00", "currency": "GBP"}, "condition": "Used", "buyingOptions": ["FIXED_PRICE"],
            "itemWebUrl": "https://www.ebay.co.uk/itm/110551111111"
        }]})),
    );
    let s = server(&fake);
    let v = json(
        &s.search_items(
            params(json!({
                "query": "lego 75192", "min_price": "300", "max_price": "700",
                "condition": "used", "buying_option": "fixed price", "item_location_country": "gb",
                "sort": "-price", "limit": 1, "offset": 10
            })),
            no_auth(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(v["items"][0]["item_id"], "v1|110551111111|0");
    assert_eq!(v["items"][0]["price"], "500.00 GBP");
    assert_eq!(v["next_offset"], 11);
    let l = fake.last("GET", "buy/browse/v1/item_summary/search");
    assert!(
        l.header("authorization")
            .unwrap()
            .starts_with("Bearer app-")
    );
    assert_eq!(l.header("x-ebay-c-marketplace-id"), Some("EBAY_GB"));
    assert_eq!(l.param("q"), Some("lego 75192"));
    assert_eq!(l.param("sort"), Some("-price"));
    assert_eq!(
        l.param("filter"),
        Some(
            "price:[300.00..700.00],priceCurrency:GBP,conditions:{USED},buyingOptions:{FIXED_PRICE},itemLocationCountry:GB"
        )
    );
    let e = error_text(&s.search_items(params(json!({})), no_auth()).await.unwrap());
    assert!(e.contains("give query"), "{e}");
}

#[tokio::test]
async fn get_item_by_number_or_id() {
    let fake = FakeEbay::start().await;
    let item = json!({"itemId": "v1|110551111111|0", "title": "Falcon", "description": "<b>Mint</b>", "localizedAspects": [{"name": "Brand", "value": "LEGO"}]});
    fake.on(
        "GET",
        "buy/browse/v1/item/get_item_by_legacy_id",
        Canned::ok(item.clone()),
    );
    fake.on(
        "GET",
        "buy/browse/v1/item/v1%7C110551111111%7C0",
        Canned::ok(item),
    );
    let s = server(&fake);
    let v = json(
        &s.get_item(params(json!({"item_id": "110551111111"})), no_auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["description"], "Mint");
    assert_eq!(v["item_specifics"]["Brand"], "LEGO");
    assert_eq!(
        fake.last("GET", "buy/browse/v1/item/get_item_by_legacy_id")
            .param("legacy_item_id"),
        Some("110551111111")
    );
    let v = json(
        &s.get_item(params(json!({"item_id": "v1|110551111111|0"})), no_auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["title"], "Falcon");
}

#[tokio::test]
async fn categories_and_their_requirements() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "commerce/taxonomy/v1/category_tree/3/get_category_suggestions",
        Canned::ok(json!({"categorySuggestions": [{
            "category": {"categoryId": "183448", "categoryName": "LEGO Complete Sets & Packs"},
            "categoryTreeNodeAncestors": [
                {"categoryName": "LEGO Building Toys", "categoryTreeNodeLevel": 2},
                {"categoryName": "Toys & Games", "categoryTreeNodeLevel": 1}
            ]
        }]})),
    );
    fake.on(
        "GET",
        "commerce/taxonomy/v1/category_tree/3/get_item_aspects_for_category",
        Canned::ok(json!({"aspects": [
            {"localizedAspectName": "Brand", "aspectConstraint": {"aspectRequired": true, "aspectMode": "FREE_TEXT"}, "aspectValues": [{"localizedValue": "LEGO"}]},
            {"localizedAspectName": "Theme", "aspectConstraint": {"aspectUsage": "RECOMMENDED", "aspectMode": "SELECTION_ONLY", "itemToAspectCardinality": "MULTI"}},
            {"localizedAspectName": "Year", "aspectConstraint": {"aspectUsage": "OPTIONAL"}}
        ]})),
    );
    fake.on(
        "GET",
        "sell/metadata/v1/marketplace/EBAY_GB/get_item_condition_policies",
        Canned::ok(json!({"itemConditionPolicies": [{"categoryId": "183448", "itemConditionRequired": true, "itemConditions": [
            {"conditionId": "1000", "conditionDescription": "New"}, {"conditionId": "3000", "conditionDescription": "Used"}
        ]}]})),
    );
    let s = server(&fake);
    let v = json(
        &s.suggest_categories(params(json!({"query": "lego falcon"})), no_auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["items"][0]["category_id"], "183448");
    assert_eq!(
        v["items"][0]["path"],
        "Toys & Games > LEGO Building Toys > LEGO Complete Sets & Packs"
    );
    let v = json(
        &s.get_category_requirements(params(json!({"category_id": "183448"})), no_auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["required_aspects"][0]["name"], "Brand");
    assert_eq!(v["recommended_aspects"][0]["free_text"], false);
    assert_eq!(v["recommended_aspects"][0]["multiple_values"], true);
    assert_eq!(v["optional_aspect_count"], 1);
    assert_eq!(v["conditions"][1]["condition"], "USED_EXCELLENT");
    assert_eq!(
        fake.last(
            "GET",
            "sell/metadata/v1/marketplace/EBAY_GB/get_item_condition_policies"
        )
        .param("filter"),
        Some("categoryIds:{183448}")
    );
}

#[tokio::test]
async fn active_listings_come_from_the_trading_api() {
    let fake = FakeEbay::start().await;
    fake.on_trading(
        "GetMyeBaySelling",
        r#"<?xml version="1.0" encoding="UTF-8"?><GetMyeBaySellingResponse xmlns="urn:ebay:apis:eBLBaseComponents"><Ack>Success</Ack>
        <ActiveList><ItemArray><Item><ItemID>123456789012</ItemID><Title>Lamp &amp; shade</Title><ListingType>FixedPriceItem</ListingType>
        <Quantity>3</Quantity><QuantityAvailable>2</QuantityAvailable><SellingStatus><CurrentPrice currencyID="GBP">40.0</CurrentPrice><QuantitySold>1</QuantitySold></SellingStatus>
        <WatchCount>4</WatchCount><TimeLeft>P20DT3H</TimeLeft></Item></ItemArray>
        <PaginationResult><TotalNumberOfPages>2</TotalNumberOfPages><TotalNumberOfEntries>60</TotalNumberOfEntries></PaginationResult></ActiveList>
        </GetMyeBaySellingResponse>"#,
    );
    let s = server(&fake);
    let v = json(
        &s.list_active_listings(params(json!({})), auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["total"], 60);
    assert_eq!(v["next_page"], 2);
    assert_eq!(v["items"][0]["title"], "Lamp & shade");
    assert_eq!(v["items"][0]["available"], 2);
    assert_eq!(v["items"][0]["price"], "40.0 GBP");
    let call = fake.last_trading("GetMyeBaySelling");
    assert!(
        call.raw_body
            .contains("<ActiveList><Include>true</Include>"),
        "{}",
        call.raw_body
    );
    assert_eq!(call.header("x-ebay-api-compatibility-level"), Some("1349"));
}

#[tokio::test]
async fn revise_listing_escapes_what_it_sends() {
    let fake = FakeEbay::start().await;
    fake.on_trading(
        "ReviseFixedPriceItem",
        r#"<?xml version="1.0"?><ReviseFixedPriceItemResponse xmlns="urn:ebay:apis:eBLBaseComponents"><Ack>Warning</Ack><ItemID>123456789012</ItemID>
        <Errors><ShortMessage>Shipping changed.</ShortMessage><ErrorCode>21917091</ErrorCode><SeverityCode>Warning</SeverityCode></Errors></ReviseFixedPriceItemResponse>"#,
    );
    let s = server(&fake);
    let v = json(
        &s.revise_listing(
            params(json!({"item_id": "123456789012", "title": "Lamp & <shade>", "price": "35", "quantity": 2})),
            auth(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(v["changed"], json!(["title", "price", "quantity"]));
    assert_eq!(v["warnings"][0], "Shipping changed. [error 21917091]");
    let body = fake.last_trading("ReviseFixedPriceItem").raw_body;
    assert!(
        body.contains("<Item><ItemID>123456789012</ItemID><Title>Lamp &amp; &lt;shade&gt;</Title><StartPrice currencyID=\"GBP\">35.00</StartPrice><Quantity>2</Quantity></Item>"),
        "{body}"
    );
}

#[tokio::test]
async fn orders_are_listed_and_shipped() {
    let fake = FakeEbay::start().await;
    let order = json!({
        "orderId": "12-34567-89012", "orderFulfillmentStatus": "NOT_STARTED",
        "lineItems": [{"lineItemId": "1001", "title": "Lamp", "quantity": 1}, {"lineItemId": "1002", "title": "Bulb", "quantity": 2}]
    });
    fake.on(
        "GET",
        "sell/fulfillment/v1/order",
        Canned::ok(json!({"total": 1, "orders": [order.clone()]})),
    );
    fake.on(
        "GET",
        "sell/fulfillment/v1/order/12-34567-89012",
        Canned::ok(order),
    );
    fake.on(
        "POST",
        "sell/fulfillment/v1/order/12-34567-89012/shipping_fulfillment",
        Canned::status(201, Value::Null).header(
            "location",
            "https://api.ebay.com/sell/fulfillment/v1/order/12-34567-89012/shipping_fulfillment/FUL-1",
        ),
    );
    let s = server(&fake);
    let v = json(
        &s.list_orders(params(json!({"created_from": "2026-10-01"})), auth())
            .await
            .unwrap(),
    );
    assert_eq!(v["items"][0]["order_id"], "12-34567-89012");
    assert_eq!(
        fake.last("GET", "sell/fulfillment/v1/order")
            .param("filter"),
        Some(
            "orderfulfillmentstatus:{NOT_STARTED|IN_PROGRESS},creationdate:[2026-10-01T00:00:00.000Z..]"
        )
    );
    let v = json(
        &s.mark_order_shipped(
            params(json!({"order_id": "12-34567-89012", "carrier": "Royal Mail", "tracking_number": "AB123456789GB", "shipped_date": "2026-11-01"})),
            auth(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(v["fulfillment_id"], "FUL-1");
    let b = fake
        .last(
            "POST",
            "sell/fulfillment/v1/order/12-34567-89012/shipping_fulfillment",
        )
        .body
        .unwrap();
    assert_eq!(
        b,
        json!({"lineItems": [{"lineItemId": "1001", "quantity": 1}, {"lineItemId": "1002", "quantity": 2}],
               "shippedDate": "2026-11-01T00:00:00.000Z", "shippingCarrierCode": "Royal Mail", "trackingNumber": "AB123456789GB"})
    );
}

#[tokio::test]
async fn raw_requests_stay_on_ebays_apis() {
    let fake = FakeEbay::start().await;
    fake.on(
        "GET",
        "sell/account/v1/rate_table",
        Canned::ok(json!({"rateTables": []})),
    );
    let s = server(&fake);
    let v = json(
        &s.api_get(
            params(json!({"path": "/sell/account/v1/rate_table", "query": {"country_code": "GB"}})),
            auth(),
        )
        .await
        .unwrap(),
    );
    assert_eq!(v["status"], 200);
    assert_eq!(
        fake.last("GET", "sell/account/v1/rate_table")
            .param("country_code"),
        Some("GB")
    );
    for bad in [
        "identity/v1/oauth2/token",
        "ws/api.dll",
        "sell/../identity/v1/oauth2/token",
        "https://evil.example.com/sell/x",
        "sell/x?y=1",
        "sell/%2e%2e/x",
    ] {
        let e = error_text(
            &s.api_get(params(json!({"path": bad})), auth())
                .await
                .unwrap(),
        );
        assert!(e.starts_with("validation: "), "{bad}: {e}");
    }
}
