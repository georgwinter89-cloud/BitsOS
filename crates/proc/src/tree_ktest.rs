//! EINS-C (K5) ktest `fs.tree.assistant_exec`: the execve rule. `/bin/assist` whose parent holds the screen's
//! compositor grant or runs `/boot/BITSOS2/bin/bitscomp` (bitscomp starting `assist serve`) runs in a fresh tree; another program of the compositor,
//! and `/bin/assist` of a parent without the grant (Georg's terminal), do not. Held processes only (nothing runs);
//! the grant is the test parent's and taken back at the end.

use crate::ktests::{PROG, SEC, image};
use crate::{Process, SpawnOptions};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use ktest::{TestResult, kassert, ktest};
use sched::interactive::{self, Grant};

// nanosleep({100, 0}); jmp .
const SLEEPER: [u8; 18] = [0x6a, 0x00, 0x6a, 0x64, 0x48, 0x89, 0xe7, 0x31, 0xf6, 0xb8, 0x23, 0, 0, 0, 0x0f, 0x05, 0xeb, 0xfe];

struct Held(Arc<Process>);
impl Drop for Held {
    fn drop(&mut self) {
        self.0.kill(9);
        let _ = self.0.wait(Some(5 * SEC));
    }
}

fn held(path: &str, parent: Option<&Arc<Process>>) -> Result<Held, String> {
    let img = image(&SLEEPER, true);
    let argv = [path];
    let mut o = SpawnOptions::new(&argv, &[], PROG);
    o.held = true;
    o.parent = parent;
    crate::spawn_image_with(path, &img, &o).map(Held).map_err(|e| format!("spawn {path}: {e}"))
}

fn assistant_exec() -> TestResult {
    let comp = held("/bin/fs-tree-compositor", None)?;
    let plain = held("/bin/fs-tree-terminal", None)?;
    interactive::set_grant(comp.0.pid as u64, Grant::Compositor, true).map_err(|e| format!("grant: {e:?}"))?;
    let r = cases(&comp.0, &plain.0);
    let _ = interactive::set_grant(comp.0.pid as u64, Grant::Compositor, false);
    r
}

fn cases(comp: &Arc<Process>, plain: &Arc<Process>) -> TestResult {
    let assist = held("/bin/assist", Some(comp))?;
    let other = held("/bin/foot", Some(comp))?;
    let georgs = held("/bin/assist", Some(plain))?;
    for h in [&assist, &other, &georgs] {
        crate::tree::on_exec(&h.0);
    }
    // bitscomp's own program counts without the grant (a refused grant does not leave the service untagged)
    let bitscomp = held("/boot/BITSOS2/bin/bitscomp", None)?;
    let served = held("/bin/assist", Some(&bitscomp.0))?;
    crate::tree::on_exec(&served.0);
    kassert!(served.0.group().tree_id() != 0, "/bin/assist of /boot/BITSOS2/bin/bitscomp without the grant is in no tree");
    let id = assist.0.group().tree_id();
    kassert!(id != 0, "the compositor's /bin/assist is in no tree");
    kassert!(assist.0.group().tree().is_some_and(|t| t.root == assist.0.pid), "the tree's root is not the service");
    kassert!(other.0.group().tree_id() == 0, "another program of the compositor came into a tree");
    kassert!(georgs.0.group().tree_id() == 0, "/bin/assist from Georg's terminal came into a tree");
    kassert!(comp.group().tree_id() == 0, "the compositor came into the tree");
    // a second execve keeps the same tree
    crate::tree::on_exec(&assist.0);
    kassert!(assist.0.group().tree_id() == id, "the tree changed at a second execve");
    Ok(())
}
ktest!("fs.tree.assistant_exec", Safe, assistant_exec);
