//! EINS-C: the process layer's answers for the trash (`vfs::trash`): a tree process's command name and chat
//! label for an entry's record, and who writes to `/dev/bitsos/papierkorb`; the access log's tree mark.

use alloc::string::String;

fn tell(pid: u32) -> Option<(String, Option<String>)> {
    let p = proc::current().filter(|p| p.pid == pid).or_else(|| proc::find(pid))?;
    Some((p.name.clone(), p.group().chat().map(|c| String::from(&*c))))
}

fn caller() -> Option<vfs::trash::Caller> {
    let p = proc::current()?;
    Some(vfs::trash::Caller { pid: p.pid, tree: p.group().tree_id(), cred: p.fs_cred(), program: p.subject().program })
}

fn tree_of(pid: u32) -> u32 {
    proc::find(pid).map_or(0, |p| p.group().tree_id())
}

fn start() -> Result<(), &'static str> {
    vfs::trash::set_teller(tell);
    vfs::trash::set_caller(caller);
    vfs::access::set_tree_info(tree_of);
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "vfs.tree", start);
