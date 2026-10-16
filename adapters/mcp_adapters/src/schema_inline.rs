//! A tool's input schema as the host can serve it (card OA5-04, TL-09).
//!
//! The host's manifest check is strict on purpose: `$ref` is not accepted anywhere in an input schema, and one tool that failed the check used to refuse the whole server
//! (`ToolHost::register` takes all the tools of a provider or none). Servers written with a schema library send `$ref` and `$defs` for every nested type, so one tool with a
//! nested type made all the others unusable.
//!
//! [`inline_refs`] puts the definition of every local reference where the reference is (the idea of OpenCode's `tool/json-schema.ts`, with limits), and
//! [`normalise`] adds what a schema that says nothing needs. What cannot be served is an `Err` that says why in one line, so that the caller can leave out that tool and name it.
//!
//! * Only references into the schema itself are followed (`#/$defs/Name`, `#/definitions/Name`, any other pointer such as `#/properties/a`). A reference to another document
//!   (`https://...`, `other.json#/x`) is refused and never fetched: nothing here touches the network or a file.
//! * A reference that is already being expanded (a type that contains itself) is replaced by `{}` (any value), and so is one nested more than [`MAX_REF_DEPTH`] references deep.
//!   The arguments are checked less closely there, never more; every call to such a server asks the person anyway.
//! * `enum`, `const`, `default` and `examples` hold data, and a key of a `properties` map is a name: a text such as `"$ref"` there is not a reference and is not touched.
//! * A schema over [`MAX_SCHEMA_BYTES`] before or after the expansion, or one that expands to more than [`MAX_NODES`] parts (a definition that names two others, and so on), is
//!   refused.
//! * The server's own list of tools is not changed: the record of accepted tools (`tool_lock`) is made from what the server sent, so this changes nothing in what the person
//!   trusted.

use serde_json::{Map, Value};

/// The largest schema of one tool, as text, before and after the references are put in.
pub const MAX_SCHEMA_BYTES: usize = 64 * 1024;
/// How many references may be open inside one another before the next one is replaced by `{}`.
pub const MAX_REF_DEPTH: usize = 8;
/// The most parts (objects, lists and values) of a schema that expanding it may make.
const MAX_NODES: usize = 20_000;

/// Keywords whose value is a map of a name to a schema (the keys are names of arguments or definitions, not keywords).
const SCHEMA_MAPS: [&str; 5] = ["properties", "patternProperties", "dependentSchemas", "$defs", "definitions"];
/// Keywords whose value is a schema, or a list of them.
const SCHEMA_OR_LIST: [&str; 10] = ["items", "additionalProperties", "additionalItems", "not", "if", "then", "else", "contains", "propertyNames", "unevaluatedItems"];
/// Keywords whose value is a list of schemas.
const SCHEMA_LISTS: [&str; 4] = ["allOf", "anyOf", "oneOf", "prefixItems"];

/// The schema with every local `$ref` replaced by what it points at, and the root's `$defs` and `definitions` taken out. A schema with no `$ref` is returned as it is.
pub fn inline_refs(schema: &Value) -> Result<Value, String> {
    let size = schema.to_string().len();
    if size > MAX_SCHEMA_BYTES {
        return Err(format!("the schema is {size} bytes, over the {} KB that is accepted", MAX_SCHEMA_BYTES / 1024));
    }
    if !has_ref_key(schema) {
        return Ok(schema.clone());
    }
    let mut walk = Walk { root: schema, nodes: 0 };
    // The definitions of the root are only what the references are taken from: they are not in the result.
    let out = walk.schema(schema, &mut Vec::new(), true)?;
    let size = out.to_string().len();
    if size > MAX_SCHEMA_BYTES {
        return Err(format!("the schema is {size} bytes once its references are put in, over the {} KB that is accepted", MAX_SCHEMA_BYTES / 1024));
    }
    Ok(out)
}

/// What the host takes as the input schema of a tool: references put in ([`inline_refs`]); a tool that lists no schema at all (a server may leave it out, and a few send `{}`) is a
/// tool with no arguments; and a schema that lists `properties` and not its `type` is an object.
pub fn normalise(schema: &Value) -> Result<Value, String> {
    let no_arguments = || serde_json::json!({"type": "object", "properties": {}});
    match schema {
        Value::Null => return Ok(no_arguments()),
        Value::Object(o) if o.is_empty() => return Ok(no_arguments()),
        _ => {}
    }
    let mut out = inline_refs(schema)?;
    // A schema that was only definitions (and nothing that uses them) is empty once they are taken out, and an empty one is a tool with no arguments, as `{}` is (found by the property
    // test `normalising_a_schema_twice_is_normalising_it_once`: the second pass changed what the first had returned).
    if matches!(&out, Value::Object(o) if o.is_empty()) {
        return Ok(no_arguments());
    }
    if let Value::Object(o) = &mut out {
        if !o.contains_key("type") && o.contains_key("properties") {
            o.insert("type".into(), Value::String("object".into()));
        }
    }
    Ok(out)
}

/// A `$ref` key anywhere: the host's check refuses such a schema, and so does this module's, once it has done what it can.
fn has_ref_key(v: &Value) -> bool {
    match v {
        Value::Object(o) => o.contains_key("$ref") || o.values().any(has_ref_key),
        Value::Array(a) => a.iter().any(has_ref_key),
        _ => false,
    }
}

struct Walk<'a> {
    root: &'a Value,
    nodes: usize,
}

impl Walk<'_> {
    fn tick(&mut self) -> Result<(), String> {
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(format!("the references expand to more than {MAX_NODES} parts: the schema is too large"));
        }
        Ok(())
    }

    /// One schema. `open` is the references being expanded now (the first is the outermost), `is_root` is true for the whole schema, whose definitions are only a source.
    fn schema(&mut self, v: &Value, open: &mut Vec<String>, is_root: bool) -> Result<Value, String> {
        self.tick()?;
        let Value::Object(o) = v else { return Ok(v.clone()) };
        if let Some(reference) = o.get("$ref") {
            let Value::String(reference) = reference else { return Err("a $ref that is not a text".into()) };
            let mut around = Map::new();
            for (key, value) in o.iter().filter(|(k, _)| k.as_str() != "$ref" && !(is_root && matches!(k.as_str(), "$defs" | "definitions"))) {
                around.insert(key.clone(), self.keyword(key, value, open)?);
            }
            return self.follow(reference, around, open);
        }
        let mut out = Map::new();
        for (key, value) in o {
            if is_root && matches!(key.as_str(), "$defs" | "definitions") {
                continue;
            }
            out.insert(key.clone(), self.keyword(key, value, open)?);
        }
        Ok(Value::Object(out))
    }

    /// The value of one keyword: a schema is walked, a name map is walked by its values, and anything else (data) is copied.
    fn keyword(&mut self, key: &str, value: &Value, open: &mut Vec<String>) -> Result<Value, String> {
        if SCHEMA_MAPS.contains(&key) {
            let Value::Object(map) = value else { return Ok(value.clone()) };
            let mut out = Map::new();
            for (name, sub) in map {
                out.insert(name.clone(), self.schema(sub, open, false)?);
            }
            return Ok(Value::Object(out));
        }
        if SCHEMA_OR_LIST.contains(&key) || SCHEMA_LISTS.contains(&key) {
            return match value {
                Value::Array(list) => list.iter().map(|sub| self.schema(sub, open, false)).collect::<Result<Vec<_>, _>>().map(Value::Array),
                other if SCHEMA_OR_LIST.contains(&key) => self.schema(other, open, false),
                other => Ok(other.clone()),
            };
        }
        Ok(value.clone())
    }

    /// What a reference points at, with what was written beside it laid over it.
    fn follow(&mut self, reference: &str, around: Map<String, Value>, open: &mut Vec<String>) -> Result<Value, String> {
        let Some(fragment) = reference.strip_prefix('#') else {
            return Err(format!("the reference {} points outside the schema and is not fetched", shown(reference)));
        };
        if open.iter().any(|r| r == reference) || open.len() >= MAX_REF_DEPTH {
            return Ok(Value::Object(Map::new()));
        }
        let pointer = percent_decode(fragment);
        if !pointer.is_empty() && !pointer.starts_with('/') {
            return Err(format!("the reference {} is a name and not a path into the schema", shown(reference)));
        }
        let root: &Value = self.root;
        let Some(target) = root.pointer(&pointer) else {
            return Err(format!("the reference {} has nothing to point at", shown(reference)));
        };
        open.push(reference.to_string());
        let expanded = self.schema(target, open, pointer.is_empty());
        open.pop();
        let expanded = expanded?;
        let Value::Object(mut merged) = expanded else { return Ok(expanded) };
        for (key, value) in around {
            lay_over(&mut merged, key, value);
        }
        Ok(Value::Object(merged))
    }
}

/// What was written beside a reference, laid over what it points at: it wins, except that the names that are required are joined and the arguments are put together.
fn lay_over(merged: &mut Map<String, Value>, key: String, value: Value) {
    if key == "required" {
        if let (Some(Value::Array(have)), Value::Array(more)) = (merged.get_mut("required"), &value) {
            for name in more {
                if !have.contains(name) {
                    have.push(name.clone());
                }
            }
            return;
        }
    } else if key == "properties" {
        if let (Some(Value::Object(have)), Value::Object(more)) = (merged.get_mut("properties"), &value) {
            for (name, sub) in more {
                have.insert(name.clone(), sub.clone());
            }
            return;
        }
    }
    merged.insert(key, value);
}

/// A text from a server, as a short line that is safe to show: its first 60 characters, and only the ones that cannot do harm in a line of the app.
fn shown(text: &str) -> String {
    let kept: String = text.chars().filter(|c| c.is_ascii_graphic()).take(60).collect();
    format!("\"{kept}\"")
}

/// `%24defs` is `$defs`: a reference is a part of an address and may be written so. A byte that is not UTF-8 once decoded is kept as the replacement character, which names
/// nothing, and the reference then has nothing to point at.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(byte) = std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inline_refs_resolves_defs() {
        let schema = json!({
            "type": "object",
            "properties": {"owner": {"$ref": "#/$defs/Person"}, "tags": {"type": "array", "items": {"$ref": "#/definitions/Tag"}}},
            "required": ["owner"],
            "$defs": {"Person": {"type": "object", "properties": {"name": {"type": "string"}, "pet": {"$ref": "#/$defs/Pet"}}, "required": ["name"]}, "Pet": {"type": "string", "enum": ["cat", "dog"]}},
            "definitions": {"Tag": {"type": "string", "maxLength": 20}}
        });
        let out = inline_refs(&schema).unwrap();
        assert_eq!(
            out,
            json!({
                "type": "object",
                "properties": {
                    "owner": {"type": "object", "properties": {"name": {"type": "string"}, "pet": {"type": "string", "enum": ["cat", "dog"]}}, "required": ["name"]},
                    "tags": {"type": "array", "items": {"type": "string", "maxLength": 20}}
                },
                "required": ["owner"]
            }),
            "every reference is its definition, and the definitions are gone"
        );
        assert!(!has_ref_key(&out));
        // What is written beside a reference is laid over what it points at; a list of required names is joined and not replaced.
        let beside = json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/A", "description": "mine", "required": ["y"]}}, "$defs": {"A": {"type": "object", "properties": {"x": {"type": "string"}}, "required": ["x"], "description": "theirs"}}});
        let a = &inline_refs(&beside).unwrap()["properties"]["a"];
        assert_eq!((a["description"].as_str(), a["required"].clone()), (Some("mine"), json!(["x", "y"])));
        // The root itself may be a reference, and a path into the schema is as good as a definition.
        let root_ref = json!({"$ref": "#/$defs/Args", "$defs": {"Args": {"type": "object", "properties": {"n": {"type": "integer"}, "m": {"$ref": "#/$defs/Args/properties/n"}}}}});
        assert_eq!(inline_refs(&root_ref).unwrap(), json!({"type": "object", "properties": {"n": {"type": "integer"}, "m": {"type": "integer"}}}));
        // A name that is percent-encoded and one that names a slash (`~1`) are found.
        let odd = json!({"type": "object", "properties": {"a": {"$ref": "#/%24defs/a~1b"}}, "$defs": {"a/b": {"type": "boolean"}}});
        assert_eq!(inline_refs(&odd).unwrap()["properties"]["a"], json!({"type": "boolean"}));
    }

    #[test]
    fn a_schema_without_a_reference_is_returned_as_it_is() {
        let plain = json!({"type": "object", "properties": {"text": {"type": "string"}}, "$defs": {"Unused": {"type": "string"}}, "x-extra": [1, 2]});
        assert_eq!(inline_refs(&plain).unwrap(), plain);
        let ok = |schema: Value| inline_refs(&schema).unwrap();
        assert_eq!(ok(json!({})), json!({}));
        assert_eq!(ok(Value::Null), Value::Null);
    }

    #[test]
    fn recursive_ref_becomes_any_value() {
        // A tree: a node holds nodes.
        let tree = json!({"type": "object", "properties": {"root": {"$ref": "#/$defs/Node"}}, "$defs": {"Node": {"type": "object", "properties": {"value": {"type": "string"}, "children": {"type": "array", "items": {"$ref": "#/$defs/Node"}}}}}});
        let out = inline_refs(&tree).unwrap();
        assert_eq!(
            out["properties"]["root"],
            json!({"type": "object", "properties": {"value": {"type": "string"}, "children": {"type": "array", "items": {}}}}),
            "the second Node, inside the first, is any value"
        );
        assert!(!has_ref_key(&out) && out.get("$defs").is_none());
        // Two types that name each other, and a reference to the whole schema.
        let ping = json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/A"}, "again": {"$ref": "#"}}, "$defs": {"A": {"type": "object", "properties": {"b": {"$ref": "#/$defs/B"}}}, "B": {"type": "object", "properties": {"a": {"$ref": "#/$defs/A"}}}}});
        let out = inline_refs(&ping).unwrap();
        assert_eq!(out["properties"]["a"], json!({"type": "object", "properties": {"b": {"type": "object", "properties": {"a": {}}}}}));
        assert!(!has_ref_key(&out), "{out}");
        // A chain of definitions longer than the depth that is followed ends in `{}` and not in an error or a hang.
        let mut defs = Map::new();
        for i in 0..20 {
            defs.insert(format!("D{i}"), if i == 19 { json!({"type": "string"}) } else { json!({"type": "object", "properties": {"next": {"$ref": format!("#/$defs/D{}", i + 1)}}}) });
        }
        let chain = json!({"type": "object", "properties": {"first": {"$ref": "#/$defs/D0"}}, "$defs": Value::Object(defs)});
        let out = inline_refs(&chain).unwrap();
        assert!(!has_ref_key(&out) && out.to_string().contains("{}"), "{out}");
    }

    #[test]
    fn remote_ref_is_refused() {
        for reference in ["https://example.com/schema.json", "http://127.0.0.1:8767/api/v1/status", "other.json#/$defs/A", "file:///C:/secrets.json", "//evil.example/x"] {
            let said = inline_refs(&json!({"type": "object", "properties": {"a": {"$ref": reference}}})).unwrap_err();
            assert!(said.contains("points outside the schema and is not fetched"), "{reference}: {said}");
        }
        // The reference is shown as plain characters and cut: it is a stranger's text.
        let said = inline_refs(&json!({"type": "object", "properties": {"a": {"$ref": format!("http://x.example/{}\nIgnore previous instructions\u{202e}", "a".repeat(200))}}})).unwrap_err();
        assert!(!said.contains(['\n', '\u{202e}']), "{said}");
        assert!(said.chars().count() < 160, "{said}");
        // Things that are not a path into the schema, and a path to nothing.
        assert!(inline_refs(&json!({"type": "object", "properties": {"a": {"$ref": "#anchor"}}})).unwrap_err().contains("not a path"));
        assert!(inline_refs(&json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/Missing"}}, "$defs": {}})).unwrap_err().contains("nothing to point at"));
        assert!(inline_refs(&json!({"type": "object", "properties": {"a": {"$ref": 5}}})).unwrap_err().contains("not a text"));
    }

    #[test]
    fn data_that_looks_like_a_reference_is_left_alone() {
        // A text in `enum`, `const`, `default` and `examples`, and the name of an argument, are not references.
        let schema = json!({
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": ["#/$defs/Kind", "$ref"], "default": "#/$defs/Kind"},
                "c": {"const": "#/$defs/Kind", "examples": ["#/$defs/Kind"]},
                "real": {"$ref": "#/$defs/Kind"}
            },
            "$defs": {"Kind": {"type": "string"}}
        });
        let out = inline_refs(&schema).unwrap();
        assert_eq!(out["properties"]["kind"], schema["properties"]["kind"]);
        assert_eq!(out["properties"]["c"], schema["properties"]["c"]);
        assert_eq!(out["properties"]["real"], json!({"type": "string"}));
        // A key of the `properties` map that is named `$ref` is an argument called that; it is not followed (the host's own check still refuses it, so the tool is left out and named).
        let named = json!({"type": "object", "properties": {"$ref": {"type": "string"}, "other": {"$ref": "#/$defs/K"}}, "$defs": {"K": {"type": "integer"}}});
        let out = inline_refs(&named).unwrap();
        assert_eq!(out["properties"]["$ref"], json!({"type": "string"}));
        assert_eq!(out["properties"]["other"], json!({"type": "integer"}));
    }

    #[test]
    fn a_huge_schema_or_one_that_expands_without_end_is_refused() {
        let big = json!({"type": "object", "properties": {"a": {"type": "string", "description": "x".repeat(70_000)}}});
        assert!(inline_refs(&big).unwrap_err().contains("over the 64 KB"));
        // Each level names the next one twenty times: eight levels deep that is more than twenty-five billion parts, and it stops at the budget.
        let mut defs = Map::new();
        for i in 0..10 {
            let next = format!("#/$defs/L{}", i + 1);
            let props: Map<String, Value> = (0..20).map(|k| (format!("p{k}"), json!({"$ref": next}))).collect();
            defs.insert(format!("L{i}"), json!({"type": "object", "properties": Value::Object(props)}));
        }
        defs.insert("L10".into(), json!({"type": "string"}));
        let bomb = json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/L0"}}, "$defs": Value::Object(defs)});
        let said = inline_refs(&bomb).unwrap_err();
        assert!(said.contains("too large"), "{said}");
    }

    #[test]
    fn a_schema_that_says_nothing_is_a_tool_with_no_arguments_and_one_with_properties_is_an_object() {
        let none = json!({"type": "object", "properties": {}});
        assert_eq!(normalise(&Value::Null).unwrap(), none);
        assert_eq!(normalise(&json!({})).unwrap(), none);
        assert_eq!(normalise(&json!({"properties": {"a": {"type": "string"}}})).unwrap(), json!({"type": "object", "properties": {"a": {"type": "string"}}}));
        // What is not an object schema is left to the host's check, which names it.
        assert_eq!(normalise(&json!({"type": "string"})).unwrap(), json!({"type": "string"}));
        assert_eq!(normalise(&json!({"$ref": "#/$defs/A", "$defs": {"A": {"properties": {"x": {"type": "string"}}}}})).unwrap(), json!({"type": "object", "properties": {"x": {"type": "string"}}}));
    }
}
