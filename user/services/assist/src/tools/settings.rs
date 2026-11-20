//! The six settings tools (`settings_list/describe/get/set/undo/history`) through settingsd's socket. Names,
//! descriptions, schemas and the mapping to socket requests are settingsd's own (`settingsd::mcp_tools`), so the
//! model sees exactly the tools Claude Code sees through the kernel's MCP endpoint.

use super::{Class, Grant, ToolAnswer, ToolSource, ToolSpec};
use crate::json::{self, Json};
use crate::settings::Settings;
use settingsd::mcp_tools;

/// The journal's `source` of a change made in the chat.
pub const SOURCE: &str = "assist";

pub struct SettingsTools<'a> {
    pub settings: &'a dyn Settings,
}

impl ToolSource for SettingsTools<'_> {
    fn list(&self) -> Vec<ToolSpec> {
        mcp_tools::tools()
            .into_iter()
            .filter_map(|t| {
                let class = match t.safety {
                    mcpreg::Safety::Read => Class::Read,
                    mcpreg::Safety::Change => Class::Change,
                    mcpreg::Safety::DeleteOverwrite => Class::Other,
                };
                let schema = json::parse(&t.input_schema.to_string()).ok()?;
                Some(ToolSpec { name: t.name, description: t.description, schema, class })
            })
            .collect()
    }

    fn call(&self, name: &str, args: &Json, grant: Grant) -> ToolAnswer {
        // `confirmed` is never the model's to say: whatever it sent is replaced by what Georg answered.
        let mut args = args.clone();
        if let Json::Obj(fields) = &mut args {
            fields.retain(|(k, _)| k != "confirmed");
            if grant.confirmed {
                fields.push(("confirmed".into(), Json::Bool(true)));
            }
        }
        let wire = mcpreg::Json::parse(&args.to_text()).map_err(|e| format!("bad_request: {e}"))?;
        let mut req = mcp_tools::request(name, &wire).map_err(|e| format!("bad_request: {e}"))?;
        if let Json::Obj(fields) = &mut req {
            for (k, v) in fields.iter_mut() {
                if k == "source" {
                    *v = Json::from(SOURCE);
                }
            }
            // The undo is held to the change the permission step (and Georg's yes) was about: a newer
            // change made in between is `changed_meanwhile` and nothing is undone.
            if let (true, Some(seq)) = (name == "settings_undo", grant.pin) {
                fields.push(("expect_seq".into(), Json::from(seq)));
            }
        }
        let mut answer = self.settings.request(&req).map_err(|e| e.to_string())?;
        if answer.get("ok").and_then(Json::as_bool) != Some(true) {
            return Err(format!("{}: {}", answer.str("error").unwrap_or("error"), answer.str("message").unwrap_or("")));
        }
        // settings_list's `limit` is the tool's, not the socket's (as settingsd's own MCP side does it).
        let limit = args.get("limit").and_then(Json::as_i64).and_then(|n| usize::try_from(n).ok());
        if let (Some(limit), "settings_list", Json::Obj(fields)) = (limit, name, &mut answer) {
            let mut more = 0;
            for (k, v) in fields.iter_mut() {
                if let (true, Json::Arr(keys)) = (k == "keys", v) {
                    more = keys.len().saturating_sub(limit);
                    keys.truncate(limit);
                }
            }
            fields.push(("more".into(), Json::from(more)));
        }
        Ok(answer.to_text())
    }
}
