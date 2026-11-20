//! L7-SYNC: `fixup_robust_unlock` on a started program (held before its first instruction): a user state inside
//! the try-unlock's critical section with ZF set gets its pending pointer cleared, nothing else does, and a
//! pointer that cannot be written drops the thread's robust list when it is the list's own.

use super::{CS32, CS64, fixup_robust_unlock};
use crate::ktests::{ended, need, start};
use alloc::format;
use core::sync::atomic::Ordering;
use hal::entry::{UserContext, test_programs};
use ktest::{TestResult, kassert, ktest};

const ZF: u64 = 1 << 6;

ktest!("sc.vdso.robust_fixup", Safe, robust_fixup);
fn robust_fixup() -> TestResult {
    let img = crate::ktests::image(need(test_programs::stack_report())?, true);
    let (p, _out) = start("/sc-ktest/robust-fixup", &img, &["robust-fixup"], None, true)?;
    let r = (|| -> TestResult {
        let at = p.saved_auxv.lock().chunks(2).find(|c| c.first() == Some(&uapi::auxv::AT_SYSINFO_EHDR)).and_then(|c| c.get(1).copied()).unwrap_or(0);
        kassert!(at != 0, "no vDSO");
        let t = p.group().threads().into_iter().next().ok_or("no thread")?;
        let mem = p.mm.sys_mmap(0, 4096, 3, 0x22, 0).map_err(|e| format!("mmap: {e:?}"))?;
        let get = |o: u64| {
            let mut b = [0u8; 8];
            let _ = p.mm.copy_from_user(&mut b, mem + o);
            u64::from_le_bytes(b)
        };
        let set = |o: u64, v: u64| {
            let _ = p.mm.copy_to_user(mem + o, &v.to_le_bytes());
        };
        let mut ctx = UserContext::default();
        ctx.regs.rdx = mem;
        for (rip, zf, hit, why) in [(at + CS64.0, true, true, "at the branch"), (at + CS64.1 - 1, true, true, "at the store"), (at + CS64.0, false, false, "the cmpxchg failed"), (at + CS64.1, true, false, "after the store"), (at + CS64.0 - 1, true, false, "before the section")] {
            set(0, 5);
            ctx.regs.rip = rip;
            ctx.regs.rflags = if zf { ZF | 2 } else { 2 };
            let did = fixup_robust_unlock(&p, &t, &ctx);
            kassert!(did == hit && (get(0) == 0) == hit, "{why}: fixed {did}, pending {}", get(0));
        }
        // The 32-bit pointer section clears four bytes.
        set(0, u64::MAX);
        ctx.regs.rip = at + CS32.0;
        ctx.regs.rflags = ZF | 2;
        kassert!(fixup_robust_unlock(&p, &t, &ctx) && get(0) == 0xffff_ffff_0000_0000, "32-bit pointer: {:#x}", get(0));
        // The thread's own list_op_pending, unwritable: the robust list goes.
        t.robust_list.store(0x1000, Ordering::Release);
        ctx.regs.rip = at + CS64.0;
        ctx.regs.rdx = 0x1010;
        kassert!(fixup_robust_unlock(&p, &t, &ctx), "unwritable pointer not handled");
        kassert!(t.robust_list.load(Ordering::Acquire) == 0, "the robust list was kept");
        Ok(())
    })();
    p.kill(9);
    p.release();
    let _ = ended(&p, 30);
    r
}
