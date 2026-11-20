//! EINS-C (K5): `/proc/<pid>/bitsos_tree`, BitsOS's own (Linux has no such file; it is opened by its name and not
//! listed, so a listing of `/proc/<pid>` stays Linux's). Read by anyone who may see the
//! process: `none` or `tree=<n> root=<pid> since=<ns> chat=<label or ->`, one line, so bitscomp and `bits` tell
//! the assistant from Georg's own programs by the pid a socket names. Written only by the process itself (EPERM
//! for any other writer, the kernel's included):
//! - `enter`: into a fresh tree; EPERM when it is in one (nothing takes a process out, nothing moves it);
//! - `chat <label>`: its chat label (`policy::tree::valid_chat`), inside a tree only; fork and execve keep it.

use alloc::format;
use alloc::vec::Vec;
use proc::Group;
use uapi::errno::*;
use vfs::Errno;

pub(super) fn render(g: &Group) -> Vec<u8> {
    match g.tree() {
        None => b"none\n".to_vec(),
        Some(t) => {
            let chat = g.chat();
            format!("tree={} root={} since={} chat={}\n", t.id, t.root, t.since_ns, chat.as_deref().unwrap_or("-")).into_bytes()
        }
    }
}

pub(super) fn write(g: &Group, data: &[u8]) -> Result<usize, Errno> {
    let Some(me) = proc::current().filter(|me| core::ptr::eq(&**me.group(), g)) else {
        klog::event!(Info, "proc", "tree.write_refused", tgid = g.tgid; "only the process itself writes its bitsos_tree");
        return Err(EPERM);
    };
    let text = core::str::from_utf8(data).map_err(|_| EINVAL)?.trim_end_matches('\n');
    match text.split_once(' ') {
        None if text == "enter" => {
            g.enter_tree(me.pid)?;
        }
        Some(("chat", label)) => g.set_chat(label)?,
        _ => return Err(EINVAL),
    }
    Ok(data.len())
}
