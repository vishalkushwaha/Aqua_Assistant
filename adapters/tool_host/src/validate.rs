//! Argument validation against a small, flat subset of JSON Schema.
//!
//! Supported: `type` (string, integer, number, boolean, array, object), `required`, `enum`,
//! `default`, `minimum`, `maximum`, `minLength`, `maxLength`, `minItems`, `maxItems`, `items`,
//! `properties`, `additionalProperties: false` (the default for objects). Anything else is
//! ignored on purpose: a schema keyword the host does not understand must not open a hole,
//! so unknown *arguments* are rejected rather than unknown keywords honoured.
//!
//! Safe coercions only: a numeric string for an integer or number, `"true"`/`"false"` for a
//! boolean, a single value for an array of one. Nothing is coerced to a string.
//!
//! Two more keywords, which a **native** tool's manifest may use and which are honoured only when the caller says so ([`validate_args_noted`] with `extensions`; a provider asks for it
//! with `ToolProvider::schema_extensions`, and a tool that comes from a server never does, or a server could widen what the host accepts): `"x-clamp": true` on an integer or number
//! moves a value outside `minimum`..`maximum` to the nearest bound and says so in a note, where without it the value is a problem; `"x-aliases": [..]` on a property names other
//! spellings of it (`file_path` for `path`), which are renamed before the check for unknown arguments. Both only widen the spelling: what the call is classified and asked as is the
//! normalised call (card OA5-03, TL-03).

use serde_json::{Map, Number, Value};

const MAX_DEPTH: usize = 8;

/// Arguments that passed, and what was done to make them pass (a value moved into its bounds, a spelling read as another): short sentences for the model.
#[derive(Debug, Clone, PartialEq)]
pub struct Validated {
    pub value: Value,
    pub notes: Vec<String>,
}

/// What a check collects on its way down.
struct Found {
    problems: Vec<String>,
    notes: Vec<String>,
    extensions: bool,
}

/// Returns the arguments with defaults applied and coercions made, or every problem found. The schema keywords `x-clamp` and `x-aliases` are not honoured here (see [`validate_args_noted`]).
pub fn validate_args(schema: &Value, args: &Value) -> Result<Value, Vec<String>> {
    validate_args_noted(schema, args, false).map(|v| v.value)
}

/// [`validate_args`] with the notes of what was changed, and the two keywords `x-clamp` and `x-aliases` honoured when `extensions` is true.
pub fn validate_args_noted(schema: &Value, args: &Value, extensions: bool) -> Result<Validated, Vec<String>> {
    let mut found = Found { problems: Vec::new(), notes: Vec::new(), extensions };
    let input = if args.is_null() { Value::Object(Map::new()) } else { args.clone() };
    let out = check(schema, &input, "arguments", 0, &mut found);
    if found.problems.is_empty() {
        Ok(Validated { value: out, notes: found.notes })
    } else {
        Err(found.problems)
    }
}

/// The name of a property as a note or a problem names it: the path without its first part (`arguments.offset_line` is `offset_line`).
fn short(path: &str) -> &str {
    path.strip_prefix("arguments.").unwrap_or(path)
}

/// The number of single-character changes between two names (insert, delete, replace), for "did you mean": names are short and the comparison is made for a handful of properties.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (row[j] + 1).min(above + 1).min(diagonal + usize::from(ca != cb));
            diagonal = above;
        }
    }
    row[b.len()]
}

/// The property of `props` that is within two changes of `key` (the nearest one), when there is one that the call does not give already.
fn nearest_property<'a>(key: &str, props: &'a Map<String, Value>, given: &Map<String, Value>) -> Option<&'a str> {
    let key: String = key.chars().take(60).collect::<String>().to_ascii_lowercase();
    props.keys().filter(|p| !given.contains_key(p.as_str())).map(|p| (edit_distance(&key, &p.to_ascii_lowercase()), p.as_str())).filter(|(d, _)| *d <= 2).min_by_key(|(d, p)| (*d, *p)).map(|(_, p)| p)
}

fn check(schema: &Value, value: &Value, path: &str, depth: usize, found: &mut Found) -> Value {
    if depth > MAX_DEPTH {
        found.problems.push(format!("{path}: nesting is too deep"));
        return value.clone();
    }
    let ty = schema.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let mut coerced = match ty {
        "integer" => match coerce_integer(value) {
            Some(n) => Value::Number(n),
            None => {
                found.problems.push(format!("{path}: expected an integer"));
                return value.clone();
            }
        },
        "number" => match coerce_number(value) {
            Some(n) => Value::Number(n),
            None => {
                found.problems.push(format!("{path}: expected a number"));
                return value.clone();
            }
        },
        "boolean" => match value {
            Value::Bool(_) => value.clone(),
            Value::String(s) if s == "true" => Value::Bool(true),
            Value::String(s) if s == "false" => Value::Bool(false),
            _ => {
                found.problems.push(format!("{path}: expected true or false"));
                return value.clone();
            }
        },
        "string" => match value {
            Value::String(_) => value.clone(),
            _ => {
                found.problems.push(format!("{path}: expected a string"));
                return value.clone();
            }
        },
        "array" => {
            let items: Vec<Value> = match value {
                Value::Array(a) => a.clone(),
                Value::Null => Vec::new(),
                single => vec![single.clone()],
            };
            let item_schema = schema.get("items");
            let checked: Vec<Value> = items
                .iter()
                .enumerate()
                .map(|(i, v)| match item_schema {
                    Some(s) => check(s, v, &format!("{path}[{i}]"), depth + 1, found),
                    None => v.clone(),
                })
                .collect();
            if let Some(min) = schema.get("minItems").and_then(|v| v.as_u64()) {
                if (checked.len() as u64) < min {
                    found.problems.push(format!("{path}: needs at least {min} items"));
                }
            }
            if let Some(max) = schema.get("maxItems").and_then(|v| v.as_u64()) {
                if checked.len() as u64 > max {
                    found.problems.push(format!("{path}: allows at most {max} items"));
                }
            }
            Value::Array(checked)
        }
        "object" => return check_object(schema, value, path, depth, found),
        _ => value.clone(),
    };

    // A number a native tool lets be moved into its bounds (`x-clamp`): the paging arguments of a read, where 0 for "from the start" is a slip and not a reason to fail the call.
    if found.extensions && schema.get("x-clamp") == Some(&Value::Bool(true)) {
        if let Value::Number(n) = &coerced {
            if let Some(moved) = clamp_into_bounds(schema, n, ty == "integer", path, found) {
                coerced = moved;
            }
        }
    }
    if let Some(options) = schema.get("enum").and_then(|e| e.as_array()) {
        if !options.contains(&coerced) {
            let list: Vec<String> = options.iter().map(|o| o.to_string()).collect();
            found.problems.push(format!("{path}: must be one of {}", list.join(", ")));
        }
    }
    if let Value::String(s) = &coerced {
        let n = s.chars().count() as u64;
        if let Some(min) = schema.get("minLength").and_then(|v| v.as_u64()) {
            if n < min {
                found.problems.push(format!("{path}: must be at least {min} characters"));
            }
        }
        if let Some(max) = schema.get("maxLength").and_then(|v| v.as_u64()) {
            if n > max {
                found.problems.push(format!("{path}: must be at most {max} characters"));
            }
        }
    }
    if let Value::Number(n) = &coerced {
        let x = n.as_f64().unwrap_or(f64::NAN);
        if let Some(min) = schema.get("minimum").and_then(|v| v.as_f64()) {
            if x < min {
                found.problems.push(format!("{path}: must be at least {min}"));
            }
        }
        if let Some(max) = schema.get("maximum").and_then(|v| v.as_f64()) {
            if x > max {
                found.problems.push(format!("{path}: must be at most {max}"));
            }
        }
    }
    coerced
}

/// A number outside `minimum`..`maximum` moved to the nearest bound, with a note that says so. `None`: it was inside, or the schema has no bound on that side.
fn clamp_into_bounds(schema: &Value, n: &Number, integer: bool, path: &str, found: &mut Found) -> Option<Value> {
    let x = n.as_f64()?;
    let (lo, hi) = (schema.get("minimum").and_then(|v| v.as_f64()), schema.get("maximum").and_then(|v| v.as_f64()));
    let (bound, word) = match (lo, hi) {
        (Some(lo), _) if x < lo => (lo, "raised"),
        (_, Some(hi)) if x > hi => (hi, "lowered"),
        _ => return None,
    };
    let (moved, shown) = if integer { (Value::Number(Number::from(bound as i64)), format!("{}", bound as i64)) } else { (Value::Number(Number::from_f64(bound)?), format!("{bound}")) };
    found.notes.push(format!("{} {n} was {word} to {shown}", short(path)));
    Some(moved)
}

/// The arguments of a call with the other spellings of a property renamed to it (`x-aliases`). When the call gives both, the property by its own name wins and the note says the other
/// was ignored. A name that is a property itself is never an alias, so a schema cannot rename one argument into another.
fn with_aliases(props: &Map<String, Value>, obj: &Map<String, Value>, path: &str, found: &mut Found) -> Map<String, Value> {
    let mut out = obj.clone();
    for (name, sub) in props {
        let Some(aliases) = sub.get("x-aliases").and_then(|a| a.as_array()) else { continue };
        for alias in aliases.iter().filter_map(|a| a.as_str()) {
            if alias == name || props.contains_key(alias) {
                continue;
            }
            let Some(v) = out.remove(alias) else { continue };
            let child = short(&format!("{path}.{name}")).to_string();
            if out.get(name).is_some_and(|given| !given.is_null()) {
                found.notes.push(format!("{alias} was ignored: {child} is given"));
            } else {
                found.notes.push(format!("{alias} was read as {child}"));
                out.insert(name.clone(), v);
            }
        }
    }
    out
}

fn check_object(schema: &Value, value: &Value, path: &str, depth: usize, found: &mut Found) -> Value {
    let Some(given) = value.as_object() else {
        found.problems.push(format!("{path}: expected an object"));
        return value.clone();
    };
    let empty = Map::new();
    let props = schema.get("properties").and_then(|p| p.as_object()).unwrap_or(&empty);
    let renamed;
    let obj = if found.extensions {
        renamed = with_aliases(props, given, path, found);
        &renamed
    } else {
        given
    };
    let mut out = Map::new();

    // Unknown arguments are an error: a model that invents a parameter should be told, and which one it probably meant.
    if schema.get("additionalProperties") != Some(&Value::Bool(true)) {
        for key in obj.keys() {
            if !props.contains_key(key) {
                let shown: String = key.chars().take(60).collect();
                let hint = nearest_property(key, props, obj).map(|p| format!(" (did you mean '{p}'?)")).unwrap_or_default();
                found.problems.push(format!("{path}: unknown argument '{shown}'{hint}"));
            }
        }
    }
    for (key, sub) in props {
        let child = format!("{path}.{key}");
        match obj.get(key) {
            Some(v) if !v.is_null() => {
                out.insert(key.clone(), check(sub, v, &child, depth + 1, found));
            }
            _ => {
                if let Some(default) = sub.get("default") {
                    out.insert(key.clone(), default.clone());
                }
            }
        }
    }
    if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
        for key in required.iter().filter_map(|k| k.as_str()) {
            if !out.contains_key(key) {
                found.problems.push(format!("{path}: missing required argument '{key}'"));
            }
        }
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(true)) {
        for (k, v) in obj {
            out.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
    Value::Object(out)
}

fn coerce_integer(v: &Value) -> Option<Number> {
    match v {
        Value::Number(n) if n.is_i64() || n.is_u64() => Some(n.clone()),
        Value::Number(n) => {
            let f = n.as_f64()?;
            (f.fract() == 0.0 && f.abs() < 9.0e15).then(|| Number::from(f as i64))
        }
        Value::String(s) => s.trim().parse::<i64>().ok().map(Number::from),
        _ => None,
    }
}

fn coerce_number(v: &Value) -> Option<Number> {
    match v {
        Value::Number(n) => Some(n.clone()),
        Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()).and_then(Number::from_f64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1, "maxLength": 32},
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 20},
                "mode": {"type": "string", "enum": ["read", "list"], "default": "read"},
                "recursive": {"type": "boolean"},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                "range": {"type": "object", "properties": {"start": {"type": "integer"}}, "required": ["start"]}
            },
            "required": ["path"]
        })
    }

    #[test]
    fn defaults_are_applied_and_safe_coercions_made() {
        let out = validate_args(&schema(), &json!({"path": "a.txt", "limit": "5", "recursive": "true", "tags": "x"})).unwrap();
        assert_eq!(out, json!({"path": "a.txt", "limit": 5, "mode": "read", "recursive": true, "tags": ["x"]}));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let err = validate_args(&schema(), &json!({"limit": 500, "mode": "delete", "extra": 1, "tags": ["a", "b", "c"]})).unwrap_err();
        let joined = err.join(" | ");
        for expect in ["missing required argument 'path'", "at most 100", "must be one of", "unknown argument 'extra'", "at most 2 items"] {
            assert!(joined.contains(expect), "missing '{expect}' in: {joined}");
        }
    }

    #[test]
    fn nothing_is_coerced_into_a_string_or_out_of_shape() {
        assert!(validate_args(&schema(), &json!({"path": 5})).is_err());
        assert!(validate_args(&schema(), &json!({"path": "a", "limit": "abc"})).is_err());
        assert!(validate_args(&schema(), &json!({"path": "a", "limit": 1.5})).is_err());
        assert!(validate_args(&schema(), &json!({"path": "a", "recursive": "yes"})).is_err());
        assert!(validate_args(&schema(), &json!("just a string")).is_err());
    }

    #[test]
    fn nested_objects_are_checked() {
        assert!(validate_args(&schema(), &json!({"path": "a", "range": {}})).is_err());
        assert!(validate_args(&schema(), &json!({"path": "a", "range": {"start": "3"}})).is_ok());
    }

    #[test]
    fn null_arguments_count_as_empty() {
        let s = json!({"type": "object", "properties": {"x": {"type": "integer", "default": 1}}});
        assert_eq!(validate_args(&s, &Value::Null).unwrap(), json!({"x": 1}));
    }

    #[test]
    fn deeply_nested_input_is_refused_not_recursed_into() {
        let mut v = json!({"a": 1});
        let mut s = json!({"type": "object", "properties": {"a": {"type": "integer"}}});
        for _ in 0..12 {
            v = json!({"n": v});
            s = json!({"type": "object", "properties": {"n": s}});
        }
        assert!(validate_args(&s, &v).is_err());
    }

    /// The schema of a paging tool as a native manifest writes it (card OA5-03, TL-03).
    fn paging() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "minLength": 1, "x-aliases": ["file_path", "filePath", "file"]},
                "offset_line": {"type": "integer", "minimum": 1, "default": 1, "x-clamp": true, "x-aliases": ["offset", "start_line"]},
                "max_lines": {"type": "integer", "minimum": 1, "maximum": 2000, "default": 500, "x-clamp": true, "x-aliases": ["limit", "lines"]},
                "timeout_s": {"type": "integer", "minimum": 1, "maximum": 60}
            },
            "required": ["path"]
        })
    }

    #[test]
    fn clamp_raises_zero_offset_and_reports_a_note() {
        let v = validate_args_noted(&paging(), &json!({"path": "a.rs", "offset_line": 0, "max_lines": 99999}), true).unwrap();
        assert_eq!(v.value["offset_line"], 1);
        assert_eq!(v.value["max_lines"], 2000);
        let mut notes = v.notes.clone();
        notes.sort();
        assert_eq!(notes, vec!["max_lines 99999 was lowered to 2000".to_string(), "offset_line 0 was raised to 1".to_string()]);
        // A value inside its bounds says nothing, and a number that is not marked is still a problem.
        let fine = validate_args_noted(&paging(), &json!({"path": "a.rs", "offset_line": 7}), true).unwrap();
        assert!(fine.notes.is_empty() && fine.value["offset_line"] == 7);
        let err = validate_args_noted(&paging(), &json!({"path": "a.rs", "timeout_s": 0}), true).unwrap_err();
        assert!(err.join(" ").contains("must be at least 1"), "{err:?}");
        // A string that holds a number is coerced first and then moved, and a number that is not a whole number is still not an integer.
        assert_eq!(validate_args_noted(&paging(), &json!({"path": "a.rs", "offset_line": "0"}), true).unwrap().value["offset_line"], 1);
        assert!(validate_args_noted(&paging(), &json!({"path": "a.rs", "offset_line": 1.5}), true).is_err());
    }

    #[test]
    fn alias_is_renamed_before_unknown_check() {
        let v = validate_args_noted(&paging(), &json!({"file_path": "a.rs", "start_line": 5, "limit": 10}), true).unwrap();
        assert_eq!((v.value["path"].as_str(), v.value["offset_line"].as_i64(), v.value["max_lines"].as_i64()), (Some("a.rs"), Some(5), Some(10)));
        assert!(v.value.get("file_path").is_none() && v.value.get("start_line").is_none(), "the call is the normalised one: {}", v.value);
        let mut notes = v.notes.clone();
        notes.sort();
        assert_eq!(notes, vec!["file_path was read as path".to_string(), "limit was read as max_lines".to_string(), "start_line was read as offset_line".to_string()]);
        // A name that is nobody's alias is still an unknown argument, and the nearest property is named.
        let err = validate_args_noted(&paging(), &json!({"pth": "a.rs"}), true).unwrap_err().join("; ");
        assert!(err.contains("unknown argument 'pth' (did you mean 'path'?)") && err.contains("missing required argument 'path'"), "{err}");
    }

    #[test]
    fn canonical_wins_over_alias() {
        let v = validate_args_noted(&paging(), &json!({"path": "real.rs", "file_path": "other.rs"}), true).unwrap();
        assert_eq!(v.value["path"], "real.rs");
        assert_eq!(v.notes, vec!["file_path was ignored: path is given".to_string()]);
    }

    #[test]
    fn x_keys_from_mcp_schemas_are_ignored() {
        // Without the switch the two keywords mean nothing: a server's schema cannot widen what the host accepts.
        let err = validate_args(&paging(), &json!({"path": "a.rs", "offset_line": 0})).unwrap_err().join("; ");
        assert!(err.contains("offset_line: must be at least 1"), "{err}");
        let err = validate_args(&paging(), &json!({"file_path": "a.rs"})).unwrap_err().join("; ");
        assert!(err.contains("unknown argument 'file_path'") && err.contains("missing required argument 'path'"), "{err}");
        let plain = validate_args_noted(&paging(), &json!({"path": "a.rs"}), false).unwrap();
        assert!(plain.notes.is_empty());
    }

    #[test]
    fn a_name_that_is_a_property_is_never_an_alias() {
        // A schema that lists one of its own properties as the alias of another does not rename it.
        let s = json!({"type": "object", "properties": {"a": {"type": "string", "x-aliases": ["b"]}, "b": {"type": "string"}}});
        let v = validate_args_noted(&s, &json!({"b": "x"}), true).unwrap();
        assert_eq!(v.value, json!({"b": "x"}));
        assert!(v.notes.is_empty());
    }

    #[test]
    fn the_nearest_property_is_named_only_when_it_is_close_and_not_given() {
        let s = json!({"type": "object", "properties": {"path": {"type": "string"}, "pattern": {"type": "string"}}});
        let err = validate_args(&s, &json!({"pth": "a"})).unwrap_err().join("; ");
        assert!(err.contains("did you mean 'path'?"), "{err}");
        let far = validate_args(&s, &json!({"zzzzzz": "a"})).unwrap_err().join("; ");
        assert!(!far.contains("did you mean"), "{far}");
        // `path` is given already, so a second near miss is not pointed at it.
        let both = validate_args(&s, &json!({"path": "a", "pth": "b"})).unwrap_err().join("; ");
        assert!(!both.contains("did you mean 'path'"), "{both}");
        assert_eq!((edit_distance("path", "pth"), edit_distance("", "ab"), edit_distance("kitten", "sitting")), (1, 2, 3));
    }
}
