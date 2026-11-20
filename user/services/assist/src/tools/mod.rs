//! The tools the model may call: the six settings tools, reached through settingsd's own socket
//! ([`settings::SettingsTools`]), and the machine's own tools from the kernel's local tools socket
//! ([`local::LocalTools`], CL-T), the desktop's `program_start` ([`desktop::Desktop`], CL-6), the context
//! service's search and workspaces ([`context::ContextTools`], CX-5), the native tools that do what Claude
//! Code's do ([`native`], EINS K3) and the desktop tools a view runs ([`desk`]).
//! [`both::Tools`] is them as the one list a chat works with.

pub mod both;
pub mod context;
pub mod desk;
pub mod describe;
pub mod desktop;
pub mod helper;
pub mod local;
pub mod native;
pub mod permit;
pub mod schema;
pub mod settings;

use crate::conv::Image;
use crate::json::Json;
use permit::Level;

/// What a tool can do, as far as permissions care. `Other` is everything that deletes, overwrites, runs a
/// program or reaches outside settings and search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Read,
    Change,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the arguments.
    pub schema: Json,
    pub class: Class,
}

/// A tool's answer for the model: `Ok` its result, `Err` what went wrong (also sent to the model, marked).
pub type ToolAnswer = Result<String, String>;

/// What the permission step hands a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grant {
    /// Georg said yes to this very call.
    pub confirmed: bool,
    /// The state the permission step looked at (`permit::Verdict::pin`); the tool does nothing when it moved.
    pub pin: Option<u64>,
    /// The level the question works under: a tool that is judged elsewhere (the compositor's `handeln`) gets it.
    pub level: Level,
}

impl Default for Grant {
    fn default() -> Grant {
        Grant { confirmed: false, pin: None, level: Level::Ask }
    }
}

impl Grant {
    pub fn confirmed(confirmed: bool) -> Grant {
        Grant { confirmed, ..Grant::default() }
    }
}

pub trait ToolSource {
    fn list(&self) -> Vec<ToolSpec>;
    /// Runs one call whose arguments passed the schema and the permission table.
    fn call(&self, name: &str, args: &Json, grant: Grant) -> ToolAnswer;
    /// The same for a call that can take long: `stop` is asked while it runs (Georg pressed Esc), and a
    /// source that waits for someone else gives the call up when it says yes.
    fn call_with(&self, name: &str, args: &Json, grant: Grant, _stop: &dyn Fn() -> bool) -> ToolAnswer {
        self.call(name, args, grant)
    }
    /// The arguments of a call as they will run, made before anybody is asked about it: where a tool would
    /// read the model's words loosely (a program's name), the question must name what really runs. Err: the
    /// call cannot run, and nobody is asked.
    fn settle(&self, _name: &str, args: &Json) -> Result<Json, String> {
        Ok(args.clone())
    }
    /// What there is to say about these tools when a chat starts: a sentence for Georg (German, when tools he
    /// would expect are not there) and a technical line for the log.
    fn greeting(&self) -> (Option<String>, Option<String>) {
        (None, None)
    }
    /// The pictures calls brought since the last look (a window, the screen), for the model with their results.
    fn take_pictures(&self) -> Vec<Image> {
        Vec::new()
    }
}
