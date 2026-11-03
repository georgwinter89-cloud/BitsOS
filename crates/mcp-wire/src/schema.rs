//! The argument check every `tools/call` gets before its handler runs: required names present, values of the
//! types the tool's own JSON schema names. `null` counts as absent (V1). Anything finer is the handler's job
//! (`ToolError::BadArgument`).

use crate::Json;
use alloc::format;
use alloc::string::String;

fn type_ok(t: &str, v: &Json) -> bool {
    match t {
        "string" => matches!(v, Json::Str(_)),
        "integer" => matches!(v, Json::Num(n) if n.is_finite() && (*n as i64) as f64 == *n),
        "number" => matches!(v, Json::Num(_)),
        "boolean" => matches!(v, Json::Bool(_)),
        "object" => matches!(v, Json::Obj(_)),
        "array" => matches!(v, Json::Arr(_)),
        "null" => v.is_null(),
        // A type this check does not know is the handler's to judge.
        _ => true,
    }
}

fn types_ok(t: &Json, v: &Json) -> bool {
    match t {
        Json::Str(s) => type_ok(s, v),
        Json::Arr(a) => a.iter().any(|t| t.as_str().is_some_and(|s| type_ok(s, v))),
        _ => true,
    }
}

/// `Err(message)` naming the argument last, as V1 did ("Missing argument: lines").
pub fn check_args(schema: &Json, args: &Json) -> Result<(), String> {
    let present = |name: &str| args.get(name).is_some_and(|v| !v.is_null());
    if let Some(req) = schema.get("required").and_then(Json::as_array) {
        for name in req.iter().filter_map(Json::as_str) {
            if !present(name) {
                return Err(format!("Missing argument: {name}"));
            }
        }
    }
    if let Some(Json::Obj(props)) = schema.get("properties") {
        for (name, spec) in props {
            if let (Some(v), Some(t)) = (args.get(name), spec.get("type"))
                && !v.is_null()
                && !types_ok(t, v)
            {
                return Err(format!("Wrong type for argument: {name}"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn required_and_types() {
        let s = Json::parse(r#"{"type":"object","properties":{"lines":{"type":"string"},"n":{"type":["integer","string"]},"f":{"type":"number"}},"required":["lines"]}"#).unwrap();
        let a = |t: &str| Json::parse(t).unwrap();
        assert_eq!(check_args(&s, &a("{}")), Err("Missing argument: lines".into()));
        assert_eq!(check_args(&s, &a(r#"{"lines":null}"#)), Err("Missing argument: lines".into()));
        assert_eq!(check_args(&s, &a(r#"{"lines":5}"#)), Err("Wrong type for argument: lines".into()));
        assert_eq!(check_args(&s, &a(r#"{"lines":"x","n":1.5}"#)), Err("Wrong type for argument: n".into()));
        assert!(check_args(&s, &a(r#"{"lines":"x","n":"0x2A","f":1.5,"other":true}"#)).is_ok());
        assert!(check_args(&s, &a(r#"{"lines":"x","n":7,"f":null}"#)).is_ok());
    }
}
