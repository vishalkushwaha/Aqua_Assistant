//! Two small pure functions that were copied into several places and kept in step by hand (card OA13-03, task AQ-14 of the OpenCode audit).
//!
//! * [`canonical_json`]: a JSON value as text with the keys of every object in order, so that the same value written with its keys in another order is the same text. It feeds the digest of a
//!   call ([`crate::call_digest`]: loop detection and the keys of a consent), the hash that pins the tools of an MCP server (`aqua_mcp_adapters::tool_lock::fingerprint`: a changed text
//!   would invalidate every pin a person made) and the key of a flow run that is started twice (`api_gateway::flow_api`). There were three copies of the same algorithm; the output of the three
//!   is proved equal to this one by the tests below, on thirty-odd values, and two hashes made with the old code are pinned.
//! * [`hex_lower`]: the bytes as lower-case hex, which nine private copies in the workspace spelled out each time.

use serde_json::Value;

/// A canonical text of a value, with the keys of every object in order: the same value is the same text whatever order its keys came in. Numbers, strings and the rest are written as
/// `serde_json` writes them.
pub fn canonical_json(v: &Value) -> String {
    match v {
        Value::Object(o) => {
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys.iter().map(|k| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), canonical_json(&o[*k]))).collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(a) => format!("[{}]", a.iter().map(canonical_json).collect::<Vec<_>>().join(",")),
        other => other.to_string(),
    }
}

/// The bytes as lower-case hexadecimal, two digits for each.
pub fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // The three copies this function replaced, as they were (the one of `host.rs`, the one of `flow_api.rs` and the one of `tool_lock.rs`), so that the proof that they agree does not
    // depend on this file alone.
    fn old_host(v: &Value) -> String {
        match v {
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                let parts: Vec<String> = keys.iter().map(|k| format!("{}:{}", serde_json::to_string(k).unwrap_or_default(), old_host(&o[*k]))).collect();
                format!("{{{}}}", parts.join(","))
            }
            Value::Array(a) => format!("[{}]", a.iter().map(old_host).collect::<Vec<_>>().join(",")),
            other => other.to_string(),
        }
    }

    fn old_flow_api(v: &Value) -> String {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                format!("{{{}}}", keys.into_iter().map(|k| format!("{}:{}", Value::String(k.clone()), old_flow_api(&m[k]))).collect::<Vec<_>>().join(","))
            }
            Value::Array(a) => format!("[{}]", a.iter().map(old_flow_api).collect::<Vec<_>>().join(",")),
            other => other.to_string(),
        }
    }

    fn old_tool_lock(v: &Value, out: &mut String) {
        match v {
            Value::Object(map) => {
                out.push('{');
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(k.clone()).to_string());
                    out.push(':');
                    old_tool_lock(&map[k], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    old_tool_lock(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }

    fn samples() -> Vec<Value> {
        let mut deep = json!(1);
        for i in 0..12 {
            deep = if i % 2 == 0 { json!({ "k": deep, "a": [i] }) } else { json!([deep, { "z": i }]) };
        }
        vec![
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(-7),
            json!(3.5),
            json!(-0.0001),
            json!(1.5e300),
            json!(18446744073709551615u64),
            json!(i64::MIN),
            json!(""),
            json!("plain"),
            json!("quote \" backslash \\ slash / tab \t newline \n return \r bell \u{7}"),
            json!("accents \u{e9}\u{e8} and a smiley \u{1f600} and a CJK \u{4e2d}\u{6587}"),
            json!("</script> <untrusted:abc> \u{2028}\u{2029}"),
            json!([]),
            json!({}),
            json!([[], {}, [[]], [{}]]),
            json!([3, 1, 2]),
            json!({"b": 1, "a": 2}),
            json!({"a": 2, "b": 1}),
            json!({"B": 1, "a": 2, "_": 3, "0": 4, "": 5}),
            json!({"\u{e9}": 1, "e": 2, "z": 3, "\u{1f600}": 4, "\u{4e2d}": 5}),
            json!({"key with \"quotes\" and \\ slashes": {"nested \n key": [1, 2, {"c": null, "a": [true, false]}]}}),
            json!({"type": "object", "properties": {"element": {"type": "string"}, "ref": {"type": "string"}}, "required": ["element"]}),
            json!({"path": "src/main.rs", "offset_line": 10, "max_lines": 80}),
            json!({"items": [{"text": "one", "status": "done"}, {"status": "pending", "text": "two"}]}),
            json!({"a": {"b": {"c": {"d": {"e": {"f": [1, [2, [3, [4, [5]]]]]}}}}}}),
            json!([{"z": 1, "y": [{"x": 1, "w": 2}]}, [{"v": {"u": null}}]]),
            json!({"n": [0, -1, 1.0, 1e-9, 12345678901234567890u64, 0.1, 0.30000000000000004]}),
            json!({"enum": [null, true, "x\ny", [1, {"k": 2, "a": 1}]]}),
            deep,
        ]
    }

    #[test]
    fn the_shared_text_is_the_same_as_the_text_of_each_of_the_three_copies_it_replaced() {
        let samples = samples();
        assert!(samples.len() >= 30, "{} samples", samples.len());
        for v in &samples {
            let now = canonical_json(v);
            assert_eq!(now, old_host(v), "host.rs, for {v}");
            assert_eq!(now, old_flow_api(v), "flow_api.rs, for {v}");
            let mut old = String::new();
            old_tool_lock(v, &mut old);
            assert_eq!(now, old, "tool_lock.rs, for {v}");
        }
    }

    #[test]
    fn keys_are_in_order_whatever_order_they_came_in_and_the_text_is_compact() {
        let a: Value = serde_json::from_str(r#"{"b": [1, {"y": 1, "x": 2}], "a": "q"}"#).unwrap();
        let b: Value = serde_json::from_str(r#"{"a": "q", "b": [1, {"x": 2, "y": 1}]}"#).unwrap();
        assert_eq!(canonical_json(&a), r#"{"a":"q","b":[1,{"x":2,"y":1}]}"#);
        assert_eq!(canonical_json(&a), canonical_json(&b));
        // Order inside an array is the array's own and is kept.
        assert_ne!(canonical_json(&json!([1, 2])), canonical_json(&json!([2, 1])));
    }

    #[test]
    fn a_call_digest_is_the_hash_it_always_was() {
        // Pinned with the code as it was before the copy was removed: the digest keys loop detection and the consent of a call, and the short one is kept in the record of a run.
        let args = json!({"path": "a/\u{e9}.rs", "opts": {"z": [3, 2.5, null, {"b": 1, "a": [true]}], "a": "x\ty"}, "n": -0.0001, "big": 18446744073709551615u64});
        assert_eq!(crate::host::call_digest("fs.read", &args), "041402d5754b5c0d24e0779c3e6cb5ea0f9d662d22d7e587ac4858654a42b076");
        assert_eq!(crate::host::short_digest("fs.read", &args), "041402d5754b5c0d");
    }

    #[test]
    fn hex_is_lower_case_two_digits_a_byte_and_empty_for_nothing() {
        assert_eq!(hex_lower(&[]), "");
        assert_eq!(hex_lower(&[0x00, 0x0f, 0x10, 0xab, 0xff]), "000f10abff");
        // What each private copy wrote: `format!("{b:02x}")` for every byte.
        let all: Vec<u8> = (0..=255u8).collect();
        let old: String = all.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex_lower(&all), old);
        assert_eq!(hex_lower(&all).len(), 512);
    }
}
