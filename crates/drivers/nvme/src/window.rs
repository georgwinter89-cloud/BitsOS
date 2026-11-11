//! NV-2p at the driver: at every boot each namespace's GPT is checked against CONFIG's two keys; only a disk that
//! passes everything gets its window armed, in the block layer and in the controller's command builder. Any doubt:
//! the disk stays read-only as a whole, with a clear Warn, and /data stays on the stick (fs-ext4 data.rs).

use crate::Nvme;
use crate::ctrl::Controller;
use crate::hw::Hw;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use block::gpt::window::{self, Boot, HEAD_BYTES, Key, Why, Window};
use block::{BlockError, Disk};
use devmodel::block::IoPriority;
use ksync::SpinLock;

/// The disk the boot's window was given to.
static CLAIMED: SpinLock<Option<String>> = SpinLock::named("nvme.window.claimed", None);

/// The file-system signature at the start of `first..first + blocks` of `disk`; Err: it could not be read.
pub fn read_signature(disk: &Arc<Disk>, first: u64, blocks: u64) -> Result<Option<&'static str>, BlockError> {
    let bs = disk.block_size() as u64;
    let head_blocks = (HEAD_BYTES as u64).div_ceil(bs).min(blocks);
    let head = disk.read_vec(first, (head_blocks * bs) as usize, IoPriority::Normal)?;
    let tail_lba = first + blocks - 1;
    let tail = disk.read_vec(tail_lba, bs as usize, IoPriority::Normal)?;
    let head_is_blank = window::signature(&head, &[]).is_none();
    if head_is_blank && window::signature(&[], &tail).is_some() && left_over(disk, first, tail_lba, &tail, bs as u32)? {
        klog::event!(Warn, "nvme", "setup.left_over_backup", disk = disk.name(), first = first; "the last block is the NTFS backup of the shrunk partition before this one, not a file system here");
        return Ok(None);
    }
    Ok(window::signature(&head, &tail))
}

/// `window::left_over_backup` against the partition that ends right before `first`.
fn left_over(disk: &Arc<Disk>, first: u64, tail_lba: u64, tail: &[u8], bs: u32) -> Result<bool, BlockError> {
    let scan = disk.scan();
    let Some(before) = scan.gpt.as_ref().and_then(|g| g.entries.iter().find(|e| e.last + 1 == first)) else { return Ok(false) };
    let boot = disk.read_vec(before.first, bs as usize, IoPriority::Normal)?;
    Ok(window::left_over_backup(before.first, &boot, tail_lba, tail, bs))
}

/// The serial Identify Controller gave.
pub fn serial_of<H: Hw>(c: &Controller<H>) -> String {
    c.info().ident.map(|i| i.serial).unwrap_or_default()
}

/// The whole check of one namespace against `key`: `gpt::window::validate` over the disk's table and signature.
pub fn check<H: Hw>(c: &Controller<H>, disk: &Arc<Disk>, key: &Key) -> Result<Window, Why> {
    let serial = serial_of(c);
    if serial.trim() != key.serial {
        return Err(Why::OtherSerial);
    }
    let (scan, total) = (disk.scan(), disk.dev_info().blocks);
    let esp = window::esp_key()?;
    window::check_table(&key.guid, total, &scan, key, esp.as_ref())?;
    let sig = read_signature(disk, key.first, key.blocks).map_err(|e| Why::Unclear(format!("the partition's start cannot be read: {}", e.as_str())))?;
    window::validate(key, esp.as_ref(), &serial, total, &scan, sig)
}

/// The command builder's side of the boot ESP's window: opened and closed with the block layer's (`Disk::set_lower`).
struct Lower<H: Hw> {
    c: Weak<Controller<H>>,
    nsid: u32,
}

impl<H: Hw + 'static> block::LowerWindow for Lower<H>
where
    Controller<H>: Send + Sync,
{
    fn open_boot(&self, first: u64, blocks: u64) -> bool {
        self.c.upgrade().is_some_and(|c| c.open_boot_window(self.nsid, first, blocks))
    }
    fn close_boot(&self) {
        if let Some(c) = self.c.upgrade() {
            c.close_boot_window();
        }
    }
}

/// Ties the disk's boot ESP window to this controller's command builder.
pub fn tie<H: Hw + 'static>(c: &Arc<Controller<H>>, nsid: u32, disk: &Disk)
where
    Controller<H>: Send + Sync,
{
    disk.set_lower(Arc::new(Lower { c: Arc::downgrade(c), nsid }));
}

/// Opens the window, block layer and command builder (the two checks that hold whatever the other one does).
pub fn arm<H: Hw>(c: &Controller<H>, nsid: u32, disk: &Disk, w: &Window) {
    c.arm_window(nsid, w.first, w.blocks);
    disk.arm_window(w.first, w.blocks, w.esp);
}

/// Closes it again.
pub fn disarm<H: Hw>(c: &Controller<H>, disk: &Disk) {
    disk.disarm_window();
    c.disarm_window();
}

/// The boot's look at one namespace. Some: its window is armed.
pub fn boot_disk(c: &Arc<Nvme>, nsid: u32, disk: &Arc<Disk>) -> Option<Window> {
    let key = window::boot_keys().ok()?;
    match check(c, disk, &key) {
        Ok(w) => {
            // A second disk with the same serial and partition GUID (a clone): which one is meant is a doubt.
            if let Some(other) = claim(disk.name()) {
                conflict(&other, disk.name());
                return None;
            }
            tie(c, nsid, disk);
            arm(c, nsid, disk, &w);
            klog::event!(Info, "nvme", "window.valid", disk = String::from(disk.name()), number = w.number, first = w.first, blocks = w.blocks;
                "NV-2p: this one partition takes writes, the rest of the disk is refused");
            Some(w)
        }
        Err(Why::OtherSerial) => None,
        Err(why) => {
            klog::event!(Warn, "nvme", "window.refused", disk = String::from(disk.name()), why = why.word(), text = why.text();
                "the whole disk stays read-only, /data stays on the stick's data.img");
            window::set_refused(&why);
            None
        }
    }
}

/// The boot's window goes to `name`; Some: a disk that is still there has it already.
fn claim(name: &str) -> Option<String> {
    CLAIMED.lock().replace(String::from(name))
}

/// A disk went away (`unregister_disks`): the same disk coming back is no second disk.
pub fn forget(name: &str) {
    let mut c = CLAIMED.lock();
    if c.as_deref() == Some(name) {
        *c = None;
    }
}

fn conflict(a: &str, b: &str) {
    let text = format!("{a} and {b} both answer to nvme_data=");
    klog::event!(Warn, "nvme", "window.refused", why = "conflict", text = text.as_str(); "both disks stay read-only, /data stays on the stick's data.img");
    if let Some(d) = block::find(a) {
        for c in crate::controllers() {
            if a.starts_with(&format!("{}n", c.name)) {
                disarm(&*c, &d);
            }
        }
    }
    window::set_conflict(text);
}

/// The window's partition and everything else of `disk` are registered.
pub fn publish_armed(disk: &str, number: u32) {
    window::set_armed(disk, &block::gpt::part_name(disk, number));
}

/// Every controller of the PCI tree has been offered to the driver and has ended its bring-up: a key nobody answered
/// to is a refusal too. Asked when a bring-up ends and by /data's wait (`window::look`), which also covers a machine
/// with no NVMe controller at all, and a probe that failed after the last bring-up ended.
pub fn all_looked() {
    if window::boot_keys().is_ok() && matches!(window::boot_state(), Boot::Looking) && crate::looked_at_all() {
        let why = Why::OtherSerial;
        klog::event!(Warn, "nvme", "window.refused", why = why.word(), text = why.text(); "every disk stays read-only, /data stays on the stick's data.img");
        window::set_refused(&why);
    }
}

/// `/proc/bitsos/data-window`: what this boot decided, one line for machines, one for people.
fn render() -> String {
    let (write, key) = (window::write_key_on(), block::gpt::window::config_has_key());
    match window::boot_state() {
        Boot::Armed { disk, part } => {
            let (first, blocks) = block::find(&disk).and_then(|d| d.window()).unwrap_or((0, 0));
            let boot = block::find(&disk).and_then(|d| d.boot_window()).map_or_else(String::new, |(f, b)| format!(" boot_first={f} boot_blocks={b}"));
            format!("window=armed disk={disk} part={part} first={first} blocks={blocks}{boot}\n")
        }
        Boot::Refused { word, text, .. } => format!("window=refused why={word}\n{text}\n"),
        Boot::Looking if !key => String::from("window=none why=no_nvme_data\n"),
        Boot::Looking if !write => String::from("window=refused why=no_nvme_write\nCONFIG.TXT has no nvme_write=on\n"),
        Boot::Looking => String::from("window=looking\n"),
    }
}
vfs::procfile!("/proc/bitsos/data-window", render);

fn init() -> Result<(), &'static str> {
    window::set_look(all_looked);
    Ok(())
}
kreg::initcall!(kreg::Level::Subsys, "nvme.window", init);

#[cfg(test)]
pub(crate) fn claim_for_test(name: &str) -> Option<String> {
    claim(name)
}
