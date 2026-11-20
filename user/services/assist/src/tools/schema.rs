//! Checks a model's tool arguments against the tool's JSON Schema before anything runs. The part of JSON
//! Schema that tool definitions use: `type` (one or a list), `properties`, `required`, `additionalProperties`
//! (false or a schema), `items`, `enum`, `minimum`, `maximum`. A keyword it does not know is not checked.

use crate::json::Json;

fn type_name(v: &Json) -> &'static str {
    match v {
        Json::Null => "null",
        Json::Bool(_) => "boolean",
        Json::Int(_) => "integer",
        Json::Float(f) if f.fract() == 0.0 => "integer",
        Json::Float(_) => "number",
        Json::Str(_) => "string",
        Json::Arr(_) => "array",
        Json::Obj(_) => "object",
    }
}

fn is_type(v: &Json, want: &str) -> bool {
    let have = type_name(v);
    have == want || (want == "number" && have == "integer")
}

fn number(v: &Json) -> Option<f64> {
    match v {
        Json::Int(n) => Some(*n as f64),
        Json::Float(f) => Some(*f),
        _ => None,
    }
}

/// Ok, or what is wrong and where (`value: want boolean, integer or string, got array`).
pub fn check(schema: &Json, value: &Json) -> Result<(), String> {
    check_at(schema, value, "")
}

fn check_at(schema: &Json, v: &Json, at: &str) -> Result<(), String> {
    let here = if at.is_empty() { "arguments" } else { at };
    match schema.get("type") {
        Some(Json::Str(t)) if !is_type(v, t) => return Err(format!("{here}: want {t}, got {}", type_name(v))),
        Some(Json::Arr(ts)) => {
            let names: Vec<&str> = ts.iter().filter_map(Json::as_str).collect();
            if !names.iter().any(|t| is_type(v, t)) {
                return Err(format!("{here}: want {}, got {}", names.join(" or "), type_name(v)));
            }
        }
        _ => {}
    }
    if let Some(Json::Arr(allowed)) = schema.get("enum")
        && !allowed.contains(v)
    {
        return Err(format!("{here}: not one of the allowed values"));
    }
    if let Some(n) = number(v) {
        if let Some(min) = schema.get("minimum").and_then(number)
            && n < min
        {
            return Err(format!("{here}: smaller than {min}"));
        }
        if let Some(max) = schema.get("maximum").and_then(number)
            && n > max
        {
            return Err(format!("{here}: larger than {max}"));
        }
    }
    match v {
        Json::Obj(fields) => {
            if let Some(Json::Arr(required)) = schema.get("required") {
                for r in required.iter().filter_map(Json::as_str) {
                    if v.get(r).is_none() {
                        return Err(format!("{here}: {r} is missing"));
                    }
                }
            }
            let props = schema.get("properties");
            for (k, child) in fields {
                let path = if at.is_empty() { k.clone() } else { format!("{at}.{k}") };
                match (props.and_then(|p| p.get(k)), schema.get("additionalProperties")) {
                    (Some(s), _) => check_at(s, child, &path)?,
                    (None, Some(Json::Bool(false))) => return Err(format!("{path}: not an argument of this tool")),
                    (None, Some(s @ Json::Obj(_))) => check_at(s, child, &path)?,
                    (None, _) => {}
                }
            }
        }
        Json::Arr(items) => {
            if let Some(s) = schema.get("items") {
                for (i, child) in items.iter().enumerate() {
                    check_at(s, child, &format!("{here}[{i}]"))?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    #[test]
    fn the_settings_set_schema_is_enforced() {
        let schema = parse(r#"{"type":"object","properties":{"key":{"type":"string"},"value":{"type":["boolean","integer","string"]},"confirmed":{"type":"boolean"}},"required":["key","value"]}"#).unwrap();
        let ok = |t: &str| check(&schema, &parse(t).unwrap());
        assert!(ok(r#"{"key":"theme.scale","value":150}"#).is_ok());
        assert!(ok(r#"{"key":"theme.mode","value":"dark","confirmed":true}"#).is_ok());
        assert_eq!(ok(r#"{"value":1}"#).unwrap_err(), "arguments: key is missing");
        assert_eq!(ok(r#"{"key":1,"value":1}"#).unwrap_err(), "key: want string, got integer");
        assert_eq!(ok(r#"{"key":"a.b","value":[1]}"#).unwrap_err(), "value: want boolean or integer or string, got array");
        assert_eq!(ok(r#"{"key":"a.b","value":1.5}"#).unwrap_err(), "value: want boolean or integer or string, got number");
        assert_eq!(ok(r#"["a"]"#).unwrap_err(), "arguments: want object, got array");
    }

    #[test]
    fn ranges_enums_items_and_closed_objects() {
        let schema = parse(r#"{"type":"object","additionalProperties":false,"properties":{"n":{"type":"integer","minimum":1,"maximum":9},"mode":{"enum":["a","b"]},"list":{"type":"array","items":{"type":"string"}}}}"#).unwrap();
        let ok = |t: &str| check(&schema, &parse(t).unwrap());
        assert!(ok(r#"{"n":1,"mode":"b","list":["x"]}"#).is_ok());
        assert!(ok(r#"{"n":0}"#).is_err());
        assert!(ok(r#"{"n":10}"#).is_err());
        assert!(ok(r#"{"mode":"c"}"#).is_err());
        assert_eq!(ok(r#"{"list":["x",2]}"#).unwrap_err(), "list[1]: want string, got integer");
        assert_eq!(ok(r#"{"extra":1}"#).unwrap_err(), "extra: not an argument of this tool");
    }
}
