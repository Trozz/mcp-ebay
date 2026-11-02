//! The little XML the Trading API needs: escaping values into requests, and reading answers as
//! JSON. An element with only text becomes a string; one with attributes too becomes
//! `{"value": text, <attribute>: ...}`; one with child elements an object, where a repeated child
//! becomes an array. [`list`] reads a child that may appear once or many times.

use serde_json::{Map, Value};

/// `s` escaped for XML text and attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // Characters XML 1.0 cannot carry at all.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// `<name>value</name>`, escaped.
pub fn el(name: &str, value: &str) -> String {
    format!("<{name}>{}</{name}>", escape(value))
}

/// The root element of `text`, as JSON.
pub fn parse(text: &str) -> Result<Value, String> {
    let doc = roxmltree::Document::parse(text).map_err(|e| e.to_string())?;
    Ok(element(doc.root_element()))
}

fn element(node: roxmltree::Node) -> Value {
    let children: Vec<roxmltree::Node> = node.children().filter(|c| c.is_element()).collect();
    let attrs: Vec<(String, Value)> = node
        .attributes()
        .map(|a| (a.name().to_string(), Value::from(a.value())))
        .collect();
    if children.is_empty() {
        let text: String = node
            .children()
            .filter(|c| c.is_text())
            .filter_map(|c| c.text())
            .collect();
        if attrs.is_empty() {
            return Value::String(text);
        }
        let mut m: Map<String, Value> = attrs.into_iter().collect();
        m.insert("value".into(), Value::String(text));
        return Value::Object(m);
    }
    let mut m: Map<String, Value> = attrs.into_iter().collect();
    for c in children {
        let name = c.tag_name().name().to_string();
        let v = element(c);
        match m.get_mut(&name) {
            Some(Value::Array(items)) => items.push(v),
            Some(existing) => {
                let first = existing.take();
                *existing = Value::Array(vec![first, v]);
            }
            None => {
                m.insert(name, v);
            }
        }
    }
    Value::Object(m)
}

/// A child that may be absent, appear once or repeat, as a list.
pub fn list(v: Option<&Value>) -> Vec<&Value> {
    match v {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(other) => vec![other],
    }
}

/// The text of an element read by [`parse`], with or without attributes.
pub fn text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m.get("value").and_then(Value::as_str).map(String::from),
        _ => None,
    }
}

/// An amount element such as `<CurrentPrice currencyID="GBP">9.99</CurrentPrice>`, as
/// `{"value": "9.99", "currency": "GBP"}`.
pub fn amount(v: Option<&Value>) -> Option<Value> {
    let value = text(v)?;
    let mut m = Map::new();
    m.insert("value".into(), Value::from(value));
    if let Some(c) = v.and_then(|v| v.get("currencyID")).and_then(Value::as_str) {
        m.insert("currency".into(), Value::from(c));
    }
    Some(Value::Object(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_escaped() {
        assert_eq!(
            el("Title", "Tom & Jerry <DVD> \"boxed\"\u{1}"),
            "<Title>Tom &amp; Jerry &lt;DVD&gt; &quot;boxed&quot;</Title>"
        );
    }

    #[test]
    fn answers_read_as_json() {
        let v = parse(
            r#"<?xml version="1.0"?><GetMyeBaySellingResponse xmlns="urn:ebay:apis:eBLBaseComponents">
            <Ack>Success</Ack>
            <ActiveList><ItemArray>
              <Item><ItemID>1</ItemID><SellingStatus><CurrentPrice currencyID="GBP">9.99</CurrentPrice></SellingStatus></Item>
              <Item><ItemID>2</ItemID></Item>
            </ItemArray></ActiveList>
            <Errors><ShortMessage>a</ShortMessage></Errors>
            </GetMyeBaySellingResponse>"#,
        )
        .unwrap();
        assert_eq!(v["Ack"], "Success");
        let items = list(v.pointer("/ActiveList/ItemArray/Item"));
        assert_eq!(items.len(), 2);
        assert_eq!(text(items[1].get("ItemID")).as_deref(), Some("2"));
        assert_eq!(
            amount(items[0].pointer("/SellingStatus/CurrentPrice")).unwrap(),
            serde_json::json!({"value": "9.99", "currency": "GBP"})
        );
        assert_eq!(list(v.get("Errors")).len(), 1);
        assert!(list(v.get("Nothing")).is_empty());
        assert!(parse("not xml").is_err());
    }
}
