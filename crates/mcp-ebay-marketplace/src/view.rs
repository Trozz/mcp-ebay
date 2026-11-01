//! Shapes eBay answers into the compact JSON the tools return: empty values dropped, HTML
//! descriptions turned into text, and pages labelled with the next offset.

use serde_json::{Map, Value, json};

/// Removes nulls, empty strings and empty arrays or objects, recursively. `None` when nothing is
/// left. `false` and `0` are kept: they carry meaning.
pub fn prune(v: Value) -> Option<Value> {
    match v {
        Value::Null => None,
        Value::String(s) if s.trim().is_empty() => None,
        Value::Array(items) => {
            let items: Vec<Value> = items.into_iter().filter_map(prune).collect();
            (!items.is_empty()).then_some(Value::Array(items))
        }
        Value::Object(map) => {
            let map: Map<String, Value> = map
                .into_iter()
                .filter_map(|(k, v)| prune(v).map(|v| (k, v)))
                .collect();
            (!map.is_empty()).then_some(Value::Object(map))
        }
        other => Some(other),
    }
}

/// `v` pruned, or `{}`.
pub fn clean(v: Value) -> Value {
    prune(v).unwrap_or_else(|| json!({}))
}

/// A page of a list: `items`, with `count`, `total` when known, `offset`, and `next_offset` while
/// there are more.
pub fn page(items: Vec<Value>, offset: u32, total: Option<u64>) -> Value {
    let mut out = Map::new();
    out.insert("count".into(), Value::from(items.len()));
    if let Some(t) = total {
        out.insert("total".into(), Value::from(t));
    }
    out.insert("offset".into(), Value::from(offset));
    let next = offset as u64 + items.len() as u64;
    if !items.is_empty() && total.is_some_and(|t| next < t) {
        out.insert("next_offset".into(), Value::from(next));
    }
    out.insert("items".into(), Value::Array(items));
    Value::Object(out)
}

/// `{"value": "9.99", "currency": "GBP"}` as `"9.99 GBP"`.
pub fn money(v: Option<&Value>) -> Option<String> {
    let v = v?;
    let value = match v.get("value")? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    match v.get("currency").and_then(Value::as_str) {
        Some(c) => Some(format!("{value} {c}")),
        None => Some(value),
    }
}

/// HTML as plain text: tags removed (block ends become line breaks), common entities decoded,
/// blank runs collapsed, and at most `max_chars` characters.
pub fn html_text(html: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(html.len().min(max_chars * 2));
    let mut chars = html.chars().peekable();
    let mut skip_until: Option<&str> = None;
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut tag = String::new();
            for t in chars.by_ref() {
                if t == '>' {
                    break;
                }
                tag.push(t);
            }
            let name: String = tag
                .trim_start_matches('/')
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric())
                .collect::<String>()
                .to_ascii_lowercase();
            if let Some(end) = skip_until {
                if tag.starts_with('/') && name == end {
                    skip_until = None;
                }
                continue;
            }
            match name.as_str() {
                "script" | "style" if !tag.starts_with('/') => {
                    skip_until = Some(if name == "script" { "script" } else { "style" })
                }
                "br" | "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                    out.push('\n')
                }
                "td" | "th" => out.push(' '),
                _ => {}
            }
            continue;
        }
        if skip_until.is_some() {
            continue;
        }
        out.push(c);
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&pound;", "£")
        .replace("&euro;", "€")
        .replace("&amp;", "&");
    let mut lines: Vec<String> = Vec::new();
    for line in decoded.lines() {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if l.is_empty() && lines.last().is_none_or(|p| p.is_empty()) {
            continue;
        }
        lines.push(l);
    }
    let text = lines.join("\n").trim().to_string();
    if text.chars().count() > max_chars {
        let cut: String = text.chars().take(max_chars).collect();
        format!("{cut}…")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prune_keeps_false_and_zero() {
        let v = clean(json!({"a": null, "b": "", "c": [], "d": {"e": null}, "f": false, "g": 0}));
        assert_eq!(v, json!({"f": false, "g": 0}));
    }

    #[test]
    fn pages_say_where_the_next_one_starts() {
        let p = page(vec![json!(1), json!(2)], 10, Some(30));
        assert_eq!(p["next_offset"], 12);
        let p = page(vec![json!(1)], 29, Some(30));
        assert!(p.get("next_offset").is_none());
    }

    #[test]
    fn money_reads_both_shapes() {
        assert_eq!(
            money(Some(&json!({"value": "9.99", "currency": "GBP"}))).as_deref(),
            Some("9.99 GBP")
        );
        assert_eq!(money(Some(&json!({"value": 5}))).as_deref(), Some("5"));
        assert_eq!(money(None), None);
    }

    #[test]
    fn html_becomes_text() {
        let t = html_text(
            "<style>p{}</style><div><b>Great</b> condition &amp; boxed</div><p>Line&nbsp;two</p><script>x()</script>",
            100,
        );
        assert_eq!(t, "Great condition & boxed\n\nLine two");
        assert_eq!(html_text("abcdef", 3), "abc…");
    }
}
