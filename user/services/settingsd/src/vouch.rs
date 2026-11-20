//! Whose `"confirmed": true` is Georg's own yes (INT-CL-4).
//!
//! `confirmed` is a field of the request, so any program can write it. For an ordinary `confirm` key that is
//! the contract: the caller says it asked Georg, and the change can be undone. Two kinds of change take
//! something away that cannot be brought back, and that no other program of his user may take from him:
//! writing or clearing a secret that is not the caller's own, and taking a provider with stored values off
//! the list `ai.providers` (its key goes with it). For these the field counts only from a peer that can know
//! that he said yes himself:
//!
//! - the session's compositor. It shows the question and takes the yes only from a real mouse or keyboard
//!   (bitscomp `ki_ask`, `page_confirm`). Judged as `assist serve` judges its client and bitspkgd who may
//!   change: the kernel's record of the program that connected, the compositor's program
//!   (`bitsinstall::CONFIRMERS`), and its pid the holder of the screen's grant (`/proc/bitsos/compositor`),
//!   read for every request;
//! - root. The console's shell is root's, root reads and writes the files anyway (bitspkgd's rule), and what
//!   MCP starts is never root.
//!
//! Everybody else is refused with a sentence that says where to do it instead: a command line of his user,
//! the assistant (whoever drives it), settingsd's own MCP tools. A secret's owner needs none of this for its
//! own secret (`owners.rs`).
//!
//! A kernel without the holder file is a host, where tests and scenarios run: there the compositor is the
//! program `bitscomp` next to settingsd (as `builtin::HOLDERS` finds `assist`), or the compositor's program by its
//! path, and nobody holds a grant.

use crate::change::Who;
use crate::events::{Level, q};
use crate::peer::{Peer, Source};
use crate::registry::{Error, Registry, err};
use std::path::Path;
use std::sync::Arc;

/// The text of the kernel's holder file now; Ok(None) on a kernel without one, Err when it could not be read.
pub type Holder = Arc<dyn Fn() -> Result<Option<String>, String> + Send + Sync>;

/// The compositor's file name in the system's directory.
const COMPOSITOR: &str = "bitscomp";

/// Where Georg gives his own yes, for every refusal.
pub const WHERE: &str = "in den Einstellungen der Oberfläche (Seite „KI“) oder als root an der Konsole";
pub(crate) const NOT_YOURS: &str = "Die Bestätigung eines anderen Programms zählt hier nicht.";

pub fn holder_file() -> Result<Option<String>, String> {
    match std::fs::read_to_string(bitsinstall::COMPOSITOR_HOLDER) {
        Ok(t) => Ok(Some(t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", bitsinstall::COMPOSITOR_HOLDER)),
    }
}

/// May `peer` vouch for Georg's yes? Ok: who it is; Err: why not. Both for the events, never for the asker.
/// `sysdir` is where the system's programs live, `holder` the kernel's holder file.
pub fn judge(peer: &Peer, sysdir: Option<&Path>, holder: &dyn Fn() -> Result<Option<String>, String>) -> Result<String, String> {
    let Some(uid) = peer.uid else { return Err("a caller without a program of its own (settingsd's MCP tools)".into()) };
    if uid == 0 {
        return Ok(format!("root (pid {})", peer.pid));
    }
    let Some(exe) = peer.exe.as_deref().map(Path::new) else {
        return Err(format!("pid {}: the program could not be told (gone, or in a namespace of its own)", peer.pid));
    };
    match holder()? {
        // A kernel that names the grant's holder also records who connected: a program told only by a later
        // look at /proc is not taken (between connect and the look a pid can come to run something else).
        Some(_) if peer.by != Source::Kernel => Err(format!("pid {}: the kernel has no record of the program that connected", peer.pid)),
        Some(text) => bitsinstall::judge_peer(&bitsinstall::CONFIRMERS, peer.pid, uid, exe, Some(&text)),
        None if sysdir.map(|d| d.join(COMPOSITOR)).as_deref() == Some(exe) => Ok(format!("{} (pid {}, uid {uid})", exe.display(), peer.pid)),
        None => bitsinstall::judge_peer(&bitsinstall::CONFIRMERS, peer.pid, uid, exe, None),
    }
}

/// Why a peer in the assistant's process tree does not confirm (for the events).
pub(crate) const TREE: &str = "a program in the assistant's process tree";

/// PLAN K2 F3: a peer in the assistant's process tree (K5) is never Georg's yes, for an ordinary `confirm` key
/// too, except the service itself: the tree's first process running `/bin/assist`, which asks him on the desktop
/// before it sends `confirmed` (EINS-A's fixed rule). A peer settingsd cannot see any more counts as inside; on a
/// kernel without the tag (the host) nobody is.
pub(crate) fn in_tree(peer: &Peer) -> bool {
    if peer.by == Source::Given || !bitstree::kernel_knows() {
        return false;
    }
    match bitstree::of_peer(Some(peer.pid)) {
        bitstree::Tree::Out => false,
        bitstree::Tree::In { root, .. } => {
            let service = matches!(peer.exe.as_deref(), Some("/bin/assist" | "/usr/bin/assist"));
            !(service && root == Some(peer.pid))
        }
    }
}

impl Registry {
    /// For a registry held by another program (tests): what the kernel's holder file says.
    pub fn set_holder(&mut self, holder: Holder) {
        self.holder = holder;
    }

    /// Whether the asker's `confirmed` would be Georg's own yes. A caller inside settingsd (a migration, a
    /// program that holds the registry itself) has no peer and is not doubted.
    pub(crate) fn vouches(&self, asker: Option<&Peer>) -> Result<(), String> {
        if asker.is_some_and(in_tree) {
            return Err(TREE.into());
        }
        asker.map_or(Ok(()), |p| judge(p, self.sysdir.as_deref(), &*self.holder).map(|_| ()))
    }

    /// Georg's own yes for a change of `key` that only he may allow. `what` is the German sentence of what
    /// would happen. Err `needs_confirm`: the asker's word would count and it did not give it yet;
    /// `not_confirmer` (or `peer_unknown`): its word does not count, with or without `confirmed`.
    pub(crate) fn own_yes(&self, op: &str, key: &str, who: &Who, what: &str) -> Result<(), Error> {
        match self.vouches(who.asker) {
            Ok(()) if who.confirmed => Ok(()),
            Ok(()) => Err(err("needs_confirm", format!("{key}: {what} Das geht nur bestätigt (settings set --confirm, \"confirmed\": true)"))),
            Err(why) => Err(self.not_confirmer(op, key, who.asker, &why, format!("{key}: {what} Das bestätigst nur du selbst: {WHERE}. {NOT_YOURS}"))),
        }
    }

    /// The refusal of a peer whose yes does not count, with its event (the peer and the reason, never a value).
    pub(crate) fn not_confirmer(&self, op: &str, key: &str, peer: Option<&Peer>, why: &str, msg: String) -> Error {
        let shown = peer.map_or_else(|| "-".to_string(), Peer::show);
        let untold = peer.is_some_and(Peer::untold);
        let name = if untold { "peer.unknown" } else { "confirm.refused" };
        self.ev.emit(Level::Warn, name, &format!("what={op} key={} {shown} why={}", q(key), q(why)));
        if untold {
            let why = "settingsd could not tell which program asks (it must stay connected until the answer)";
            return err("peer_unknown", format!("{msg}; {why}"));
        }
        err("not_confirmer", msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(pid: u32, uid: u32, exe: Option<&str>, by: Source) -> Peer {
        Peer { pid, uid: Some(uid), exe: exe.map(str::to_string), by }
    }

    fn says(text: &'static str) -> impl Fn() -> Result<Option<String>, String> {
        move || Ok(Some(text.to_string()))
    }

    /// EINS-D CR 1: a peer of the assistant's tree never confirms; one settingsd cannot see counts as inside.
    #[test]
    fn a_peer_in_the_assistants_tree_never_confirms() {
        assert!(!in_tree(&crate::peer::INSIDE), "settingsd's own callers");
        assert!(!in_tree(&peer(std::process::id(), 1000, Some("/bin/x"), Source::Kernel)), "this test runs outside the tree");
        let mut c = std::process::Command::new("true").spawn().unwrap();
        let pid = c.id();
        c.wait().unwrap();
        let gone = peer(pid, 1000, Some("/bin/assist"), Source::Kernel);
        assert_eq!(in_tree(&gone), bitstree::kernel_knows(), "a peer that is gone: inside where the kernel keeps the tag");
    }

    #[test]
    fn on_bitsos_only_the_grant_holding_compositor_and_root_vouch() {
        let holds = says("holder=412\n");
        let sys = Some(Path::new("/bin"));
        let comp = Some("/boot/BITSOS2/bin/bitscomp");
        assert!(judge(&peer(412, 1000, comp, Source::Kernel), sys, &holds).is_ok());
        assert!(judge(&peer(9, 0, Some("/bin/settings"), Source::Kernel), sys, &holds).is_ok(), "root, whatever it runs");
        assert!(judge(&peer(9, 0, None, Source::Kernel), sys, &|| Err("EIO".into())).is_ok(), "root needs no holder file");
        let refused: [(Peer, &dyn Fn() -> Result<Option<String>, String>, &str); 11] = [
            (peer(500, 1000, Some("/bin/settings"), Source::Kernel), &holds, "the command line of his user"),
            (peer(500, 1000, Some("/bin/assist"), Source::Kernel), &holds, "the assistant, whoever drives it"),
            (peer(413, 1000, comp, Source::Kernel), &holds, "another bitscomp than the screen's"),
            (peer(412, 1000, Some("/bin/settings"), Source::Kernel), &holds, "another program with the holder's pid"),
            (peer(412, 1000, comp, Source::Proc), &holds, "told by /proc where the kernel keeps the record"),
            (peer(412, 1000, comp, Source::Given), &holds, "vouched for by nobody the kernel knows"),
            (peer(412, 1000, None, Source::Kernel), &holds, "a program in a namespace of its own"),
            (peer(412, 1001, comp, Source::Kernel), &holds, "another user's compositor"),
            (peer(412, 1000, comp, Source::Kernel), &says("holder=none\n"), "nobody holds the grant"),
            (peer(412, 1000, comp, Source::Kernel), &says("holder=x\n"), "a holder file that is not understood"),
            (peer(412, 1000, comp, Source::Kernel), &|| Err("EIO".into()), "a holder file that cannot be read"),
        ];
        for (p, holder, what) in refused {
            assert!(judge(&p, sys, holder).is_err(), "{what}");
        }
        assert!(judge(&crate::peer::INSIDE, sys, &holds).is_err(), "settingsd's own MCP tools");
    }

    #[test]
    fn on_a_host_the_compositor_is_the_one_next_to_settingsd() {
        let none = || Ok(None);
        let sys = Some(Path::new("/work/target/release"));
        assert!(judge(&peer(7, 1234, Some("/work/target/release/bitscomp"), Source::Proc), sys, &none).is_ok());
        assert!(judge(&peer(7, 1000, Some("/boot/BITSOS2/bin/bitscomp"), Source::Given), sys, &none).is_ok());
        assert!(judge(&peer(7, 0, Some("/work/target/release/settings"), Source::Proc), sys, &none).is_ok());
        for exe in ["/work/target/release/settings", "/work/target/release/assist", "/tmp/bitscomp", "/work/target/release/bitscomp-x"] {
            assert!(judge(&peer(7, 1000, Some(exe), Source::Proc), sys, &none).is_err(), "{exe}");
        }
        assert!(judge(&peer(7, 1000, Some("/work/target/release/bitscomp"), Source::Proc), None, &none).is_err());
    }
}
