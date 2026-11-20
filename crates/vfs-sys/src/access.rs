//! DB2: the process layer's answer for the access log's reader (vfs::access): a pid's program and start time.

use alloc::vec::Vec;

fn info(pid: u32) -> Option<(Vec<u8>, u64)> {
    proc::find(pid).map(|p| (p.path.as_bytes().to_vec(), p.start_ns))
}

fn start() -> Result<(), &'static str> {
    vfs::access::set_proc_info(info);
    Ok(())
}
kreg::initcall!(kreg::Level::Service, "vfs.access", start);
