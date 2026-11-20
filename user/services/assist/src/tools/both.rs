//! The settings tools, the machine's tools, the desktop's `program_start`, the context service's tools, the native
//! tools and the desktop tools a view runs as one list. The list is made once, when the chat starts: a
//! question does not go to the tools socket again to learn what it may offer. settingsd's six also stand in
//! the door's list (they are local tools too); they are offered once and stay with settingsd's own socket,
//! where the change is journaled as the assistant's and an undo is held to the change Georg said yes to.

use super::context::ContextTools;
use super::desk::{Desk, DeskTools};
use super::desktop::{self, Desktop};
use super::native::NativeTools;
use super::local::{LocalTools, Place, SOCKET, Why};
use super::settings::SettingsTools;
use super::{Grant, ToolAnswer, ToolSource, ToolSpec};
use crate::conv::Image;
use crate::json::Json;
use crate::settings::Settings;
use std::path::Path;

/// What Georg reads when the machine's tools are not there; the chat goes on with the settings tools.
pub const ONLY_SETTINGS: &str = "Die Werkzeuge für diesen Rechner sind gerade nicht erreichbar; ich kann nur die Einstellungen lesen und ändern.";

pub struct Tools<'a> {
    settings: SettingsTools<'a>,
    local: Option<LocalTools>,
    specs: Vec<ToolSpec>,
    /// How many of `specs` are settingsd's.
    own: usize,
    /// The log's line about the machine's tools; with no `local` it is why they are not there.
    log: Option<String>,
    /// The desktop of this session, when its `program_start` is in `specs`.
    desktop: Option<Desktop>,
    /// The context service (ctxd), with those of its tools that are in `specs`.
    context: Option<ContextTools>,
    /// The native tools (K3), when they are in `specs`.
    native: Option<NativeTools>,
    /// The view's desktop tools (K3), when a view shows this chat.
    desk: Option<DeskTools<'a>>,
}

impl<'a> Tools<'a> {
    pub fn new(settings: &'a dyn Settings, local: Result<LocalTools, Why>) -> Tools<'a> {
        let settings = SettingsTools { settings };
        let mut specs = settings.list();
        let own = specs.len();
        let (local, log) = match local {
            Ok(l) => {
                let more: Vec<ToolSpec> = l.specs().iter().filter(|t| !specs.iter().any(|s| s.name == t.name)).cloned().collect();
                specs.extend(more);
                let log = (l.left_out > 0).then(|| format!("tools: {} entries of the list left out (name or schema not usable, or too much text)", l.left_out));
                (Some(l), log)
            }
            Err(why) => (None, Some(why.log_line())),
        };
        Tools { settings, local, specs, own, log, desktop: None, context: None, native: None, desk: None }
    }

    /// The same with the native tools (K3) whose names the list does not have yet, for the chat `chat`.
    pub fn with_native(mut self, chat: &str) -> Tools<'a> {
        let more: Vec<ToolSpec> = super::native::specs().into_iter().filter(|t| !self.specs.iter().any(|s| s.name == t.name)).collect();
        if !more.is_empty() {
            self.specs.extend(more);
            self.native = Some(NativeTools { chat: chat.to_string() });
        }
        self
    }

    /// The same with the desktop tools of the view `desk` (K3), whose names the list does not have yet.
    pub fn with_desk(mut self, desk: Option<&'a dyn Desk>) -> Tools<'a> {
        if let Some(d) = desk {
            let more: Vec<ToolSpec> = super::desk::specs().into_iter().filter(|t| !self.specs.iter().any(|s| s.name == t.name)).collect();
            self.specs.extend(more);
            self.desk = Some(DeskTools::new(d));
        }
        self
    }

    /// The same with the desktop's `program_start`, unless the machine's list has a tool of that name.
    pub fn with_desktop(mut self, d: Option<Desktop>) -> Tools<'a> {
        if let Some(d) = d.filter(|_| !self.specs.iter().any(|t| t.name == desktop::NAME)) {
            self.specs.push(Desktop::spec());
            self.desktop = Some(d);
        }
        self
    }

    /// The same with the context service's tools whose names the list does not have yet.
    pub fn with_context(mut self, c: Option<ContextTools>) -> Tools<'a> {
        if let Some(mut c) = c {
            c.keep(|name| !self.specs.iter().any(|s| s.name == name));
            if !c.specs().is_empty() {
                self.specs.extend(c.specs().iter().cloned());
                self.context = Some(c);
            }
        }
        self
    }

    /// The tools of a chat that starts now.
    pub fn start(settings: &'a dyn Settings, place: Place, budget: usize) -> Tools<'a> {
        // The desktop only beside the system's own door: an assistant whose settings were named from outside
        // starts nothing on Georg's screen either.
        let door = matches!(place, Place::Door);
        let local = match place {
            Place::Door => LocalTools::open(Path::new(SOCKET), budget),
            Place::Named(p) => LocalTools::open_apart(&p, Path::new(SOCKET), budget),
            Place::Nowhere(why) => Err(Why::NotAsked(why)),
        };
        Tools::new(settings, local).with_desktop(Desktop::find().filter(|_| door)).with_context(door.then(ContextTools::find).flatten())
    }
}

impl ToolSource for Tools<'_> {
    fn list(&self) -> Vec<ToolSpec> {
        self.specs.clone()
    }

    fn call(&self, name: &str, args: &Json, grant: Grant) -> ToolAnswer {
        self.call_with(name, args, grant, &|| false)
    }

    fn call_with(&self, name: &str, args: &Json, grant: Grant, stop: &dyn Fn() -> bool) -> ToolAnswer {
        match (self.specs.iter().position(|t| t.name == name), &self.local, &self.desktop) {
            (Some(i), _, _) if i < self.own => self.settings.call(name, args, grant),
            (Some(_), _, Some(d)) if name == desktop::NAME => d.call(args),
            (Some(_), _, _) if let Some(c) = self.context.as_ref().filter(|c| c.has(name)) => c.call(name, args),
            (Some(_), _, _) if let Some(n) = self.native.as_ref().filter(|_| NativeTools::has(name)) => n.call_with(name, args, grant, stop),
            (Some(_), _, _) if let Some(d) = self.desk.as_ref().filter(|_| DeskTools::has(name)) => d.call_with(name, args, grant, stop),
            (Some(_), Some(local), _) => local.call(name, args, stop),
            _ => Err(format!("unknown tool {name:?}")),
        }
    }

    fn settle(&self, name: &str, args: &Json) -> Result<Json, String> {
        match &self.desktop {
            Some(d) if name == desktop::NAME => d.settle(args),
            _ => Ok(args.clone()),
        }
    }

    fn greeting(&self) -> (Option<String>, Option<String>) {
        (self.local.is_none().then(|| ONLY_SETTINGS.to_string()), self.log.clone())
    }

    fn take_pictures(&self) -> Vec<Image> {
        self.desk.as_ref().map(ToolSource::take_pictures).unwrap_or_default()
    }
}
