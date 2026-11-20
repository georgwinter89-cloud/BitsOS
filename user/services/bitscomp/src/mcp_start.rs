//! The MCP tool `program_start` (CL-6): starts a program of the program library by its name, as a click on
//! its tile does. Not a command line: the caller names an entry, and only that entry's own start line runs.
//! A name that fits several programs starts none; the answer lists them, so whoever asked (a model) says
//! which one and nobody guesses.
//!
//! The same start is the control command `program-start NAME`: the MCP tool is registered by bitscomp, a
//! service of uid 1000, so the kernel offers it on the network endpoint only, never on the local tools socket
//! BitsOS's assistant uses. The assistant reaches it through the control socket instead (assist's
//! `tools/desktop.rs`), after its own permission step. Like every control command it counts as injected input.

use mcpreg::{Content, Reply};

use crate::address::Where;
use crate::library::{Entry, Index};
use crate::state::App;

/// The most names an answer lists.
const LISTED: usize = 40;
/// The longest name taken (a library name is far shorter).
pub const NAME_MAX: usize = 200;

fn listed(entries: &[&Entry]) -> String {
    let mut names: Vec<String> = entries.iter().take(LISTED).map(|e| format!("{} (id {})", e.name, e.id)).collect();
    if entries.len() > LISTED {
        names.push(format!("and {} more", entries.len() - LISTED));
    }
    names.join(", ")
}

/// A name as a caller sends it: one line of text of a sensible length. Err: the words for the caller.
pub fn name_of(raw: &str) -> Result<&str, String> {
    let name = raw.trim();
    if name.is_empty() || name.len() > NAME_MAX || name.chars().any(char::is_control) {
        return Err("name must be the name or id of a program in the library (one line of text)".into());
    }
    Ok(name)
}

/// The entry `name` means: its id or address, else the one entry the library's own search finds for it, else
/// among several the one whose name is exactly that. Err: the words for the caller.
pub fn pick<'a>(ix: &'a Index, name: &str) -> Result<&'a Entry, String> {
    let name = name.trim();
    if let Some(e) = ix.by_address(name) {
        return Ok(e);
    }
    let hits = ix.matches(name);
    let exact: Vec<&Entry> = hits.iter().copied().filter(|e| e.name.to_lowercase() == name.to_lowercase()).collect();
    match (hits.as_slice(), exact.as_slice()) {
        ([], _) => Err(format!("no program of the library fits {name:?}. The library has: {}", listed(&ix.matches("")))),
        ([one], _) | (_, [one]) => Ok(*one),
        _ => Err(format!("several programs fit {name:?}, none was started: {}. Call again with one id.", listed(&hits))),
    }
}

impl App {
    /// `ctl program-find NAME`: the one entry a name means, as `<id>\t<name>`, starting nothing. The
    /// assistant asks Georg about that entry and then starts it by its id, so the card names what starts.
    pub(crate) fn ctl_program_find(&self, raw: &str) -> Result<String, String> {
        pick(&self.library, name_of(raw)?).map(|e| format!("{}\t{}", e.id, e.name))
    }

    /// `ctl program-start NAME`: [`App::mcp_start`] with its answer as one line.
    pub(crate) fn ctl_program_start(&mut self, raw: &str) -> Result<String, String> {
        match self.mcp_start(name_of(raw)?) {
            Reply::Ok(content) => Ok(content.iter().filter_map(|c| if let Content::Text(t) = c { Some(t.as_str()) } else { None }).collect::<Vec<_>>().join(" ")),
            Reply::Error(e) | Reply::BadArgument(e) => Err(e),
        }
    }

    pub(crate) fn mcp_start(&mut self, name: &str) -> Reply {
        let entry = match pick(&self.library, name) {
            Ok(e) => e.clone(),
            Err(why) => return Reply::bad_argument(why),
        };
        // One of bitscomp's own pages (Einstellungen, Sofa-Modus) is opened by its address.
        if entry.exec.is_empty() {
            self.open_address(&entry.address, Where::Tab, None);
            return Reply::text(format!("opened {}", entry.name));
        }
        match self.open_program(entry.argv(), Where::Tab, None) {
            Some(pid) => {
                self.recent_add(&entry.address);
                eprintln!("bitscomp: MCP program_start: {} (pid {pid})", entry.id);
                Reply::text(format!("started {} (pid {pid}); its window appears when the program has drawn it", entry.name))
            }
            None => Reply::error(format!("{} could not be started: its program was not found on this machine", entry.name)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_means_one_entry_of_the_library_or_none() {
        let ix = Index::builtin();
        let id = |name: &str| pick(&ix, name).map(|e| e.id.as_str());
        // Its id, its address, its name, a word the library's search knows it by.
        for name in ["terminal", "programm/terminal", "Terminal", " TERMINAL ", "Befehlszeile", "konsole"] {
            assert_eq!(id(name), Ok("terminal"), "{name}");
        }
        assert_eq!(id("Einstellungen"), Ok("einstellungen"), "the exact name wins over other entries that also fit");
        assert_eq!(id("sofa"), Ok("sofa-modus"));
        let none = id("gibt-es-nicht").unwrap_err();
        assert!(none.contains("no program") && none.contains("Terminal (id terminal)"), "{none}");
        let several = id("e").unwrap_err();
        assert!(several.contains("several programs") && several.contains("Einstellungen (id einstellungen)") && several.contains("Terminal (id terminal)"), "{several}");
        assert_eq!(name_of("  Brave \n"), Ok("Brave"));
        for bad in ["", "   ", "a\nb", "a\u{7}", &"x".repeat(NAME_MAX + 1)] {
            assert!(name_of(bad).is_err(), "{bad:?}");
        }
        // A word the library's search knows: the entry it means, by its own id and name.
        let found = pick(&ix, "Befehlszeile").map(|e| (e.id.clone(), e.name.clone())).unwrap();
        assert_eq!(found, ("terminal".to_string(), "Terminal".to_string()));
        assert_eq!(pick(&ix, &found.0).map(|e| e.id.as_str()), Ok("terminal"), "its id starts exactly that entry");
        // Nothing of a command line is ever an entry.
        for sly in ["terminal; rm -rf ~", "/bin/sh", "sh -c true", "../../bin/sh", "foot --help"] {
            assert!(id(sly).is_err(), "{sly}");
        }
    }
}
