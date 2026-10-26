//! The GOP framebuffer, the list of its modes, and the switch to the wanted mode before the kernel (BT-2:
//! native 2560x1440 unless `screen=` says otherwise; `bitsboot_pure::screen`). Read through the raw protocol
//! layout because uefi-rs hides mode numbers (and skips modes it cannot query).

use alloc::vec::Vec;
use bitsboot_pure::config::TestHook;
use bitsboot_pure::edid;
use bitsboot_pure::pixel::{self, bytes_per_pixel, check_framebuffer, from_uefi};
use bitsboot_pure::screen::{self, Pick, Want};
use core::sync::atomic::{AtomicU32, Ordering};
use bootinfo::layout::VMAP_BASE;
use bootinfo::{Framebuffer, Mode, PixelFormat};
use core::ptr::NonNull;
use log::{info, warn};
use uefi::proto::{ProtocolPointer, unsafe_protocol};
use uefi::{Handle, Status};
use uefi::boot::{self, OpenProtocolAttributes, OpenProtocolParams};
use uefi::proto::console::gop::GraphicsOutput;

pub struct Screen {
    pub fb: Framebuffer,
    pub modes: Vec<Mode>,
}

/// EFI_GRAPHICS_OUTPUT_PROTOCOL (UEFI 2.10 §12.9).
#[repr(C)]
struct RawGop {
    query_mode: unsafe extern "efiapi" fn(*const RawGop, u32, *mut usize, *mut *const RawInfo) -> Status,
    set_mode: unsafe extern "efiapi" fn(*const RawGop, u32) -> Status,
    blt: unsafe extern "efiapi" fn(*const RawGop, *const u32, u32, usize, usize, usize, usize, usize, usize, usize) -> Status,
    mode: *const RawMode,
}
#[repr(C)]
struct RawMode {
    max_mode: u32,
    mode: u32,
    info: *const RawInfo,
    size_of_info: usize,
    fb_base: u64,
    fb_size: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct RawInfo {
    version: u32,
    width: u32,
    height: u32,
    format: u32,
    masks: [u32; 4],
    stride: u32,
}

fn query(gop: &RawGop, index: u32) -> Option<RawInfo> {
    let (mut size, mut info) = (0usize, core::ptr::null());
    // SAFETY: QueryMode of the live protocol; it writes a pool pointer and its size into valid locals.
    if unsafe { (gop.query_mode)(gop, index, &mut size, &mut info) }.is_error() || info.is_null() {
        return None;
    }
    // SAFETY: on success `info` points to `size` bytes of pool memory; read only when it covers RawInfo.
    let out = (size >= size_of::<RawInfo>()).then(|| unsafe { info.read_unaligned() });
    // SAFETY: the firmware allocated `info` from pool for us (QueryMode contract) and nothing else uses it.
    let _ = unsafe { boot::free_pool(NonNull::new(info as *mut u8)?) };
    out
}

/// Runs `f` on the firmware's graphics output protocol; `None` when there is none (headless).
fn with_gop<T>(f: impl FnOnce(&RawGop) -> Option<T>) -> Option<T> {
    let handle = boot::get_handle_for_protocol::<GraphicsOutput>().ok()?;
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    // SAFETY: GetProtocol, not exclusive: an exclusive open would disconnect the firmware console from the
    // screen. The interface stays valid while `g` lives.
    let g = unsafe { boot::open_protocol::<GraphicsOutput>(params, OpenProtocolAttributes::GetProtocol) }.ok()?;
    // SAFETY: GraphicsOutput is a transparent wrapper of the UEFI protocol struct whose prefix RawGop mirrors.
    f(unsafe { &*(&*g as *const GraphicsOutput as *const RawGop) })
}

/// Fills rows `y..y+h` of the whole screen width with one colour through the firmware (Blt VideoFill): works
/// whatever the page tables map. A probe for machines without COM1.
pub fn fill_rows(y: usize, h: usize, rgb: u32) {
    let _ = with_gop(|g| {
        // SAFETY: the live protocol's mode block (GOP contract), read-only.
        let width = unsafe { (*(*g.mode).info).width } as usize;
        // SAFETY: Blt with EfiBltVideoFill (0) reads one pixel from `px` and fills the rectangle on screen.
        let px = rgb;
        let _ = unsafe { (g.blt)(g, &px, 0, 0, 0, 0, y, width, h, 0) };
        Some(())
    });
}

/// SetMode; `Err` carries the firmware's status.
fn set_mode(index: u32) -> Result<(), Status> {
    // SAFETY: SetMode of the live protocol with a mode number the firmware listed (QueryMode answered it).
    match with_gop(|raw| Some(unsafe { (raw.set_mode)(raw, index) })) {
        None => Err(Status::UNSUPPORTED),
        Some(s) if s.is_error() => Err(s),
        Some(_) => Ok(()),
    }
}

/// The firmware's mode before bitsboot switched (`NONE` = no switch), so a failed boot can put it back.
static FIRMWARE_MODE: AtomicU32 = AtomicU32::new(NONE);
const NONE: u32 = u32::MAX;

/// The boot failed after a switch: the firmware's own mode comes back before the error screen, so the
/// firmware's console and the next boot entry find the screen as they left it.
pub fn restore_firmware_mode() {
    let m = FIRMWARE_MODE.swap(NONE, Ordering::SeqCst);
    if m != NONE && let Err(s) = set_mode(m) {
        ::log::warn!("bitsboot screen.restore_failed: SetMode({m}) back to the firmware's mode: {s:?}");
    }
}

/// The screen as the kernel will get it: the mode `screen=` (default: native 2560x1440) asks for if the
/// firmware offers it, else the firmware's mode; every decision is a loader log line.
pub fn choose_screen(config: &[u8]) -> Option<Screen> {
    let value = bitsboot_pure::config::get(config, screen::KEY);
    let (want, note) = screen::parse(value);
    if let Some(n) = note {
        warn!("bitsboot screen.config_invalid: {n}");
    }
    let before = query_screen()?;
    let wanted = match want {
        Want::Size(w, h) => alloc::format!("{w}x{h}"),
        Want::Largest => "largest".into(),
        Want::Keep => "firmware".into(),
    };
    let (fw, cur) = (before.fb.mode_index, alloc::format!("{}x{}", before.fb.width, before.fb.height));
    let index = match screen::pick(&before.modes, fw, want) {
        Pick::Keep(why) => {
            info!("bitsboot screen.kept wanted={wanted} mode={fw} ({cur}): {why}");
            return Some(before);
        }
        Pick::Missing(why) => {
            warn!("bitsboot screen.fallback wanted={wanted} firmware_mode={fw} ({cur}): {why}");
            return Some(before);
        }
        Pick::Switch(i) => i,
    };
    // Recorded before the call: a failed SetMode may still have changed the mode, and a later boot error must
    // then put the firmware's mode back.
    FIRMWARE_MODE.store(fw, Ordering::SeqCst);
    if let Err(s) = set_mode(index) {
        let now = query_screen();
        match now.as_ref().map(|n| n.fb.mode_index) {
            Some(m) if m == fw => {
                FIRMWARE_MODE.store(NONE, Ordering::SeqCst);
                warn!("bitsboot screen.fallback wanted={wanted}: SetMode({index}) failed ({s:?}); keeping mode {fw} ({cur})");
            }
            m => warn!("bitsboot screen.fallback wanted={wanted}: SetMode({index}) failed ({s:?}) and left mode {m:?}, not {fw}"),
        }
        return now;
    }
    let after = query_screen();
    let good = after.as_ref().is_some_and(|s| s.fb.mode_index == index && s.fb.format != PixelFormat::None);
    if good {
        let a = after.as_ref().map(|s| (s.fb.width, s.fb.height)).unwrap_or_default();
        info!("bitsboot screen.switched from mode {fw} ({cur}) to mode {index} ({}x{})", a.0, a.1);
        return after;
    }
    warn!("bitsboot screen.fallback wanted={wanted}: mode {index} has no usable framebuffer; back to mode {fw} ({cur})");
    restore_firmware_mode();
    query_screen()
}

/// `None` when the firmware offers no GOP at all (headless). A mode without a usable linear framebuffer is
/// recorded with format `None` (the kernel then runs headless, serial + MCP).
pub fn query_screen() -> Option<Screen> {
    with_gop(screen_of)
}

fn screen_of(raw: &RawGop) -> Option<Screen> {
    if raw.mode.is_null() {
        warn!("graphics output without a mode structure: running headless");
        return None;
    }
    // SAFETY: `mode` is the protocol's live EFI_GRAPHICS_OUTPUT_PROTOCOL_MODE (checked non-null).
    let m = unsafe { &*raw.mode };
    if m.info.is_null() || m.size_of_info < size_of::<RawInfo>() {
        warn!("graphics output without mode information: running headless");
        return None;
    }
    // SAFETY: `info` points to `size_of_info` bytes, which cover RawInfo (checked above).
    let cur = unsafe { m.info.read_unaligned() };
    let (mut fmt, masks) = from_uefi(cur.format, cur.masks);
    let size = m.fb_size as u64;
    if fmt != PixelFormat::None
        && let Err(why) = check_framebuffer(m.fb_base, size, cur.width, cur.height, cur.stride, bytes_per_pixel(fmt, masks))
    {
        warn!("the current mode's framebuffer is not usable ({why}): running headless");
        fmt = PixelFormat::None;
    }
    let (phys, size) = if fmt == PixelFormat::None { (0, 0) } else { (m.fb_base, size) };
    let fb = Framebuffer {
        phys,
        virt: if phys == 0 { 0 } else { VMAP_BASE + (phys & 0xFFF) },
        size,
        width: cur.width,
        height: cur.height,
        stride: cur.stride,
        format: fmt,
        masks,
        mode_index: m.mode,
    };
    let modes: Vec<Mode> = (0..m.max_mode)
        .filter_map(|i| query(raw, i).map(|q| (i, q)))
        .map(|(index, q)| Mode { index, width: q.width, height: q.height, format: from_uefi(q.format, q.masks).0, stride: q.stride })
        .collect();
    Some(Screen { fb, modes })
}

pub fn log(screen: &Option<Screen>) {
    match screen {
        Some(s) => {
            info!(
                "screen {}x{} stride {} {} at {:#x} ({} bytes), mode {}",
                s.fb.width,
                s.fb.height,
                s.fb.stride,
                pixel::format_name(s.fb.format),
                s.fb.phys,
                s.fb.size,
                s.fb.mode_index
            );
            info!("screen modes: {}", pixel::mode_summary(&s.modes, s.fb.mode_index));
        }
        None => warn!("no graphics output: the kernel runs headless"),
    }
}

/// EFI_EDID_ACTIVE_PROTOCOL / EFI_EDID_DISCOVERED_PROTOCOL (UEFI 2.10 §12.9.2, one layout for both; uefi-raw
/// 0.17 `EdidDiscoveredProtocol`). uefi 0.41 has no Active protocol, so both are declared here.
#[repr(C)]
struct RawEdid {
    size_of_edid: u32,
    edid: *const u8,
}
#[unsafe_protocol("bd8c1056-9f36-44ec-92a8-a6337f817986")]
#[repr(transparent)]
struct EdidActive(RawEdid);
#[unsafe_protocol("1c0c34f6-d380-41fa-a049-8ad06c1a66aa")]
#[repr(transparent)]
struct EdidDiscovered(RawEdid);

/// The bytes of one EDID protocol on `handle`: `None` = not installed, empty = installed without an EDID.
fn edid_bytes<P: ProtocolPointer + ?Sized>(handle: Handle, raw: fn(&P) -> &RawEdid) -> Option<Vec<u8>> {
    let params = OpenProtocolParams { handle, agent: boot::image_handle(), controller: None };
    // SAFETY: GetProtocol only (never exclusive, see with_gop); the interface stays valid while `p` lives.
    let p = unsafe { boot::open_protocol::<P>(params, OpenProtocolAttributes::GetProtocol) }.ok()?;
    let r = raw(&p);
    // Never more than an EDID can be (byte 126 counts 255 extensions at most), whatever the firmware claims.
    let n = edid::claimed_len(r.edid.is_null(), r.size_of_edid);
    if n == 0 {
        return Some(Vec::new());
    }
    // SAFETY: the protocol's Edid points to SizeOfEdid readable bytes while it is installed (UEFI §12.9.2);
    // claimed_len gives n <= SizeOfEdid and n > 0 only for a non-null pointer; copied out at once.
    Some(unsafe { core::slice::from_raw_parts(r.edid, n) }.to_vec())
}

/// The monitor's EDID from the GOP handle (Active, else Discovered), checked and logged; empty = none.
/// `hook` (hypervisor only) stands in for a firmware with EDID protocols, which QEMU's OVMF does not have.
pub fn edid(hook: TestHook) -> Vec<u8> {
    let Ok(handle) = boot::get_handle_for_protocol::<GraphicsOutput>() else {
        info!("bitsboot edid.none: no graphics output");
        return Vec::new();
    };
    let mut active = edid_bytes::<EdidActive>(handle, |p| &p.0);
    let mut discovered = edid_bytes::<EdidDiscovered>(handle, |p| &p.0);
    match hook {
        TestHook::EdidActive => active = Some(edid::fixture()),
        TestHook::EdidFallback => {
            let mut broken = edid::fixture();
            broken[0] = 0xFF;
            (active, discovered) = (Some(broken), Some(edid::fixture()));
        }
        _ => {}
    }
    let p = edid::pick(active.as_deref(), discovered.as_deref());
    if edid::suspicious(&p) {
        warn!("bitsboot {}", edid::line(&p));
    } else {
        info!("bitsboot {}", edid::line(&p));
    }
    p.chosen.map(|(_, c)| c.bytes.to_vec()).unwrap_or_default()
}
