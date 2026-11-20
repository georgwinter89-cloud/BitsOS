//! The door's `tools/list` as tools for the model. The list is the kernel's, but its entries can come from a
//! service that registered them: a name becomes part of the request to the provider and of the tool lines Georg
//! reads, the class decides whether a call runs without a question. So an entry is taken
//! only when its name is a plain one and its schema an object, and it counts as "reads only" only when the
//! kernel vouches for that; everything else asks first. What is taken goes to the provider with every
//! question, so its text is bounded too: a description is cut, an oversized schema or a list beyond what a
//! request should carry is left out.

use crate::json::Json;
use crate::tools::{Class, ToolSpec};

/// Arguments the door refuses although the tool's schema lists them (`mcp::local::serve::WITHHELD`: they
/// reach the journal, which a program of Georg's user cannot read otherwise): not offered to the model.
const WITHHELD: &[(&str, &[&str])] = &[("metrics", &["since", "until"])];
/// The longest tool name every provider kind takes.
const NAME_MAX: usize = 64;
/// The most characters of a tool's description the model reads; the door's own tools use a few hundred.
pub const DESCRIPTION_MAX: usize = 2048;
/// A schema is not cut (half a schema is another schema): a tool with a longer one is left out.
pub const SCHEMA_MAX: usize = 16 * 1024;
/// The text of all tools taken from one list, which every question carries to the provider. Not a count:
/// hundreds of ordinary tools fit, a few bloated ones do not push the rest of the request out.
pub const OFFER_MAX: usize = 256 * 1024;

/// Letters, digits, `_` and `-`: what every provider takes as a tool name and what is one part of a key.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= NAME_MAX && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn class(annotations: Option<&Json>) -> Class {
    let a = |k: &str| annotations.and_then(|a| a.get(k));
    // A service of another user than root only claims its class: the kernel then lists it with
    // readOnlyHint false, and it asks like everything else.
    match (a("safety").and_then(Json::as_str), a("readOnlyHint").and_then(Json::as_bool), a("verified").and_then(Json::as_bool)) {
        (Some("read"), Some(true), verified) if verified != Some(false) => Class::Read,
        (Some("change"), _, verified) if verified != Some(false) => Class::Change,
        _ => Class::Other,
    }
}

fn without(mut schema: Json, names: &[&str]) -> Json {
    if let Json::Obj(fields) = &mut schema {
        for (k, v) in fields.iter_mut() {
            match (k.as_str(), v) {
                ("properties", Json::Obj(props)) => props.retain(|(p, _)| !names.contains(&p.as_str())),
                ("required", Json::Arr(req)) => req.retain(|r| !r.as_str().is_some_and(|r| names.contains(&r))),
                _ => {}
            }
        }
    }
    schema
}

/// The tool of one entry and the bytes of text it adds to every request.
fn spec(entry: &Json) -> Option<(ToolSpec, usize)> {
    let name = entry.str("name").filter(|n| plain_name(n))?;
    let schema = entry.get("inputSchema").filter(|s| matches!(s, Json::Obj(_)) && s.str("type") == Some("object"))?;
    let hidden = WITHHELD.iter().find(|(t, _)| *t == name).map_or(&[][..], |(_, names)| *names);
    let schema = without(schema.clone(), hidden);
    let size = schema.to_text().len();
    if size > SCHEMA_MAX {
        return None;
    }
    let description: String = entry.str("description").unwrap_or("").chars().take(DESCRIPTION_MAX).collect();
    let size = size + name.len() + description.len();
    Some((ToolSpec { name: name.to_string(), description, schema, class: class(entry.get("annotations")) }, size))
}

/// The usable tools of a `tools/list` result, each name once, and how many entries were left out.
pub fn specs(result: &Json) -> (Vec<ToolSpec>, usize) {
    let mut out: Vec<ToolSpec> = Vec::new();
    let mut left_out = 0;
    let mut text = 0;
    if let Some(Json::Arr(entries)) = result.get("tools") {
        for e in entries {
            match spec(e) {
                Some((s, size)) if text + size <= OFFER_MAX && !out.iter().any(|o| o.name == s.name) => {
                    text += size;
                    out.push(s);
                }
                _ => left_out += 1,
            }
        }
    }
    (out, left_out)
}
