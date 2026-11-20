//! The ring's stages of PSP 14.0.3: `ring` (create and the two reserved-region questions), `toc`, `tmr` (nothing
//! is sent) and `destroy`. Their register accesses are Linux's, compared with the trace's section psp-ring: on a
//! secure OS this boot loaded itself the ring stage reads nothing Linux does not read (a fresh secure OS has no
//! ring from before, and the note is made from what the stage wrote). Only on a secure OS that ran already
//! (`Sos::Warm`) does it look for a ring from before first, as it always did.

use super::fwlist;
use super::linuxseq::rules::{Address, Rule};
use super::pspfiles::SosFile;
use super::regs14::Regs14;
use super::ring14::{self, Reserved, Ring14};
use super::stage::{Ctx, Session, Sos, Step, Stop, need_pri, need_ring, psp_stop, read_file, ring_regs, stop};
use super::vram::{Bump, Layout};
use crate::gpucore::fwload::FwSource;
use crate::gpucore::handoff::{GcFate, RingFate, StopNote, read_note};
use crate::gpucore::psp::{self, Hw, PspError, cmd};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Where the ring's stages differ from Linux's trace (section psp-ring) and say so: the ring's address, a page of
/// BitsOS's own window where Linux's lay at MC 0x83_DA7A2000.
pub fn linux_rules(l: &Layout) -> [Rule; 1] {
    [Rule::Address(Address { lo: "MPASP_SMN_C2PMSG_69", hi: Some("MPASP_SMN_C2PMSG_70"), shift: 0, keep: 0, align: 4096, within: l.window_mc(), bytes: 4096, why: "the ring lies in BitsOS's own window of VRAM" })]
}

/// `psp_update_fw_reservation` asks only from this secure OS version on (amdgpu_psp.c:1083-1086), the version of
/// the SOS descriptor in the stick's file (`parse_sos_bin_descriptor` :4034-4035); below it Linux returns 0.
pub const RESERV_FROM_SOS: u32 = 0x003A_0E14;

fn note_matches(n: &StopNote, regs: [u32; 4]) -> bool {
    n.ring != RingFate::NotDestroyed && n.matches(Some(regs[0]), Some(regs[1]), Some(regs[2]), Some(regs[3]))
}

fn note(hw: &mut dyn Hw, r: &Regs14, ring: RingFate) -> StopNote {
    read_note(hw, &r.psp, ring, GcFate::Untouched, false, false)
}

/// The note of a ring that is up, from what this boot wrote: Linux reads none of the ring's registers back, so
/// neither does a stage that is compared with it.
fn up_note(ring: &Ring14) -> StopNote {
    let mc = ring.ring.buf.mc;
    StopNote { ring: RingFate::NotDestroyed, gc: GcFate::Untouched, tmr_destroyed: false, bus_master_left_on: false, wptr: ring.wptr(), size: ring.ring.bytes(), addr_lo: mc as u32, addr_hi: (mc >> 32) as u32 }
}

fn sos_file(c: &Ctx, files: &mut dyn FwSource) -> Result<(&'static str, Vec<u8>, SosFile), Stop> {
    let name = fwlist::name_for(fwlist::SOS, c.facts.device, c.facts.revision);
    let bytes = read_file(files, name)?;
    let parsed = SosFile::parse(&bytes).map_err(|e| stop("eine Datei auf dem Stick passt nicht", format!("{name}: {e:?}")))?;
    Ok((name, bytes, parsed))
}

pub(super) fn ring(s: &mut Session, c: &Ctx, hw: &mut dyn Hw, r: &Regs14, files: &mut dyn FwSource, out: &mut Vec<String>) -> Step {
    match s.ring.as_ref() {
        // The chain's next stage is `ring` again then (`Session::passed`), and `toc` is told so: say what to do.
        Some(x) if x.created() && s.reserved.is_none() => return Err(stop("der Befehlskanal ist angelegt, aber die Karte hat ihren eigenen Speicher nicht genannt: erst abbauen", "a ring exists whose reserved-region questions did not pass: destroy it, then ring again")),
        Some(x) if x.created() => return Err(stop("der Befehlskanal ist schon angelegt", "a ring exists in this boot: a second one is refused")),
        Some(_) => return Err(stop("ein halb angelegter Befehlskanal ist noch da: erst abbauen", "ring create failed after its registers went out: only destroy clears it")),
        None => {}
    }
    // The chain's order check saw a secure OS that runs (`stage::in_order`): one this boot loaded has no ring from
    // before, any other is looked at first.
    let warm = !matches!(s.sos, Some(Sos::Loaded { .. }));
    if warm {
        let regs = ring_regs(hw, r);
        let known = s.note.or(c.inherited).filter(|n| note_matches(n, regs));
        if regs[1] != 0 && known.is_none() {
            out.push(format!("ring registers: {regs:x?}"));
            return Err(stop("ein Befehlskanal von vorher ist noch angelegt: erst abbauen", "C2PMSG_71 is not 0 and no note of this card says it was destroyed"));
        }
    }
    let l = s.layout.clone().ok_or_else(|| stop("kein sicherer Platz im Speicher der Karte", "no window planned"))?;
    let (name, _, sos) = sos_file(c, files)?;
    sos.toc().map_err(|e| stop("eine Datei auf dem Stick passt nicht", format!("{name}: {e:?}")))?;
    let sos_version = sos.sos_version();
    let ask_reserved = sos_version.is_some_and(|v| v >= RESERV_FROM_SOS);
    out.push(format!("vram: mc base {:#x}, firmware picture {}, window {:#x}..{:#x}", l.mc_base, l.fb.as_ref().map_or(String::from("not on this card"), |f| format!("{:#x}..{:#x}", f.start, f.end)), l.window.start, l.window.end));
    // The firmware buffer first, as Linux has it from `psp_sw_init`: the TOC stage needs it, and so its megabyte
    // lies below the ring's pages whether `sos` made it or this stage does.
    need_pri(s)?;
    let bump = s.bump.get_or_insert_with(|| Bump::new(&l));
    let kept = bump.given.len();
    let mut ring = Ring14::new(bump, &l, r.psp, c.t).map_err(psp_stop)?;
    let made = ring.create(hw);
    if let Err(e) = made {
        // Linux destroys the ring on this path (amdgpu_psp.c:3676-3682); here the ring counts as up as soon as its
        // registers went out, whatever create answered, so only the destroy stage clears it.
        if ring.sent() {
            s.note = Some(up_note(&ring));
            s.ring = Some(ring);
        } else {
            bump.rewind(kept);
        }
        return Err(psp_stop(e));
    }
    out.push(format!("ring created at mc {:#x}, 4096 bytes", ring.ring.buf.mc));
    let ringref = s.ring.insert(ring);
    if !ask_reserved {
        out.push(format!("secure OS version {sos_version:x?} in the stick's file is below {RESERV_FROM_SOS:#x}: Linux does not ask for the reserved region, neither does this"));
        s.reserved = Some([Reserved::default(); 2]);
        let n = up_note(ringref);
        s.note = Some(n);
        return Ok(Some(n));
    }
    let res = ringref.reserved(hw);
    let n = up_note(ringref);
    match res {
        Ok(rv) => {
            out.push(format!("reserved by the firmware: {:#x} bytes at {:#x}, extended {:#x} bytes at {:#x}", rv[0].size, rv[0].base, rv[1].size, rv[1].base));
            // The firmware's own regions are kept from the manager from here on, both or neither. Neither may lie
            // over the window, the picture or anything handed out before it was known (the GART table and the
            // default page of `gmc`).
            let regions: Vec<_> = rv.iter().filter(|x| x.size != 0).map(|x| x.base..x.base.saturating_add(u64::from(x.size))).collect();
            let kept = s.bump.as_mut().map_or(Ok(()), |b| b.keep_firmware(&regions));
            if let Err(why) = kept {
                out.push(format!("the window {:#x}..{:#x} or memory handed out before overlaps a region of the firmware's ({why:x?}): the ring is destroyed again", l.window.start, l.window.end));
                let gone = ringref.destroy(hw).is_ok();
                if !gone {
                    out.push(String::from("the destroy was not answered cleanly: the ring counts as up"));
                }
                // The registers as the secure OS left them: the next ring stage holds the note against them.
                let n = if gone { note(hw, r, RingFate::Destroyed) } else { n };
                s.note = Some(n);
                if gone {
                    s.ring = None;
                }
                return Err(stop("der freie Platz im Speicher der Karte gehört der Karte selbst", "window overlaps the firmware's reserved region"));
            }
            // Only regions the manager keeps count as known: a ring stage that stopped above left none.
            s.reserved = Some(rv);
        }
        Err(e) => {
            s.note = Some(n);
            return Err(psp_stop(e));
        }
    }
    s.note = Some(n);
    Ok(Some(n))
}

pub(super) fn toc(s: &mut Session, c: &Ctx, hw: &mut dyn Hw, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    if s.tmr_size.is_some() {
        return Err(stop("Reihenfolge stimmt nicht", "toc ran for this ring: once per ring"));
    }
    let (name, bytes, sos) = sos_file(c, files)?;
    let d = sos.toc().map_err(|e| stop("eine Datei auf dem Stick passt nicht", format!("{name}: {e:?}")))?;
    let toc = &bytes[d.range.clone()];
    if toc.len() as u64 > psp::PRI_BUF_BYTES {
        return Err(stop("eine Datei auf dem Stick passt nicht", "toc larger than the firmware buffer"));
    }
    need_ring(s)?;
    let pri = need_pri(s)?;
    let mc = pri.buf.mc;
    pri.load(hw, toc).map_err(psp_stop)?;
    let a = need_ring(s)?.submit(hw, cmd::LOAD_TOC, &[mc as u32, (mc >> 32) as u32, toc.len() as u32], &[]).map_err(psp_stop)?;
    // Linux takes the size for a TMR it does not allocate on 14.0.3 (`boot_time_tmr`, amdgpu_psp.c:917): whatever
    // the secure OS names, 0 included, nothing follows from it.
    out.push(format!("toc: {} bytes of {name} (secure OS version {:x?}) at mc {mc:#x}; tmr size {:#x}", toc.len(), sos.sos_version(), a.resp.tmr_size));
    s.tmr_size = Some(a.resp.tmr_size);
    Ok(())
}

/// 14.0.3 sets `boot_time_tmr` and `autoload_supported` (amdgpu_psp.c:184-185, not cleared at :268-271): no TMR is
/// allocated (:917) and `psp_tmr_load` sends nothing (`psp_skip_tmr` :931-939). The stage only checks the order.
pub(super) fn tmr(s: &mut Session, out: &mut Vec<String>) -> Result<(), Stop> {
    if s.tmr_done {
        return Err(stop("Reihenfolge stimmt nicht", "tmr ran for this ring: once per ring"));
    }
    out.push(format!("tmr: the firmware's boot-time TMR ({:#x} bytes per the TOC); Linux sends no SETUP_TMR on 14.0.3, nothing sent", s.tmr_size.unwrap_or(0)));
    s.tmr_done = true;
    Ok(())
}

pub(super) fn destroy(s: &mut Session, hw: &mut dyn Hw, r: &Regs14, out: &mut Vec<String>) -> Result<StopNote, Stop> {
    let had = s.ring.is_some();
    if !had && ring_regs(hw, r)[1] == 0 {
        return Err(stop("es gibt keinen Befehlskanal", "no ring in this boot and C2PMSG_71 reads 0"));
    }
    let res = match s.ring.as_mut() {
        Some(ring) => ring.destroy(hw),
        None => ring14::stop(&r.psp, hw),
    };
    let ok = res.is_ok();
    let n = note(hw, r, if ok { RingFate::Destroyed } else { RingFate::NotDestroyed });
    s.note = Some(n);
    out.push(format!("destroy ({}): {res:?}; note {}", if had { "this boot's ring" } else { "a ring from before" }, n.text()));
    if !ok {
        return Err(psp_stop(res.err().unwrap_or(PspError::NoRing)));
    }
    s.after_destroy(n);
    Ok(n)
}
