//! sc.fpu.user_processes (owner: SC): lives here because it needs `mm`, and `mm` depends on `sched`.
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use hal::xsave::{self, RegDump};
use ksync::SpinLock;
use ktest::{kassert, ktest};
use sched::{spawn_with, yield_now, Class, SpawnOpts};

type R = Result<(), String>;
const SEC: u64 = 1_000_000_000;

ktest!("sc.fpu.user_processes", Safe, fpu_user_processes);

fn user_code(k: u32) -> [u8; 13] {
    let id = (0xC0DE_0000u32 | k).to_le_bytes();
    [0xC5, 0xFD, 0xD4, 0xC1, 0xB8, id[0], id[1], id[2], id[3], 0x0F, 0x05, 0xEB, 0xF3]
}

const USER_CODE: u64 = 0x40_0000;
const USER_ROUNDS: u64 = 200;

fn user_process(k: u32, order: &SpinLock<Vec<u32>>) -> Result<(), String> {
    use hal::entry::{ReturnReason, UserContext};
    let asp = mm::AddressSpace::new().map_err(|e| format!("address space: {e:?}"))?;
    asp.map_fixed(USER_CODE, 4096, mm::Prot { read: true, write: false, exec: true }, mm::VmaKind::Image).map_err(|e| format!("map: {e:?}"))?;
    asp.write_loader(USER_CODE, &user_code(k)).map_err(|e| format!("load: {e:?}"))?;
    let kernel_root = hal::paging::home_root();
    asp.activate();
    let mut ctx = UserContext::new(USER_CODE, 0);
    let mut expect = RegDump::pattern(0x5EED_0000 | k as u64);
    let mut result = Ok(());
    {
        let _irq = hal::cpu::IrqGuard::new();
        xsave::load_user_regs(&expect);
    }
    for round in 1..=USER_ROUNDS {
        match ctx.execute(false) {
            ReturnReason::Syscall => {}
            ReturnReason::Trap { vector, error, cr2 } => {
                result = Err(format!("process {k} round {round}: trap {vector} error {error:#x} cr2 {cr2:#x} rip {:#x}", ctx.regs.rip));
                break;
            }
        }
        if ctx.regs.rax != (0xC0DE_0000 | k) as u64 {
            result = Err(format!("process {k} ran code with id {:#x}: the address space was not switched", ctx.regs.rax));
            break;
        }
        // The user's vpaddq: ymm0 += ymm1 per 64-bit lane; a VEX instruction clears bits 511:256 of zmm0.
        let inc: Vec<u64> = expect.reg(1)[..32].chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap_or([0; 8]))).collect();
        let r0 = expect.reg_mut(0);
        for (lane, add) in inc.iter().enumerate() {
            let v = u64::from_le_bytes(r0[lane * 8..lane * 8 + 8].try_into().unwrap_or([0; 8])).wrapping_add(*add);
            r0[lane * 8..lane * 8 + 8].copy_from_slice(&v.to_le_bytes());
        }
        r0[32..].fill(0);
        if let Some(d) = xsave::read_user_regs().diff(&expect) {
            result = Err(format!("process {k} round {round}: {d}"));
            break;
        }
        order.lock().push(k);
        yield_now();
    }
    hal::paging::activate(kernel_root);
    drop(asp);
    result
}

/// Two user processes (own address spaces, same code address, different code) run alternately; each keeps
/// its own AVX state, which its own user code keeps changing, and its own CR3.
fn fpu_user_processes() -> R {
    if xsave::reg_shape().1 < 32 {
        return ktest::skip("no AVX in XCR0");
    }
    let errs = Arc::new(SpinLock::new(Vec::new()));
    let order = Arc::new(SpinLock::new(Vec::new()));
    let mut hs = Vec::new();
    for k in 1..=2u32 {
        let (errs, order) = (errs.clone(), order.clone());
        hs.push(
            // Both on one CPU (INT-A2a2): on two CPUs they ran side by side and "alternate" was luck (the gate
            // flake "the processes did not alternate"); on one, each `yield_now` hands over to the other.
            spawn_with("sc-user", Class::Normal, SpawnOpts { cpu: Some(0), pin: true, ..SpawnOpts::default() }, move || {
                if let Err(e) = user_process(k, &order) {
                    errs.lock().push(e);
                }
            })
            .map_err(|e| format!("{e:?}"))?,
        );
    }
    for h in &hs {
        kassert!(h.join_timeout(30 * SEC), "user processes: thread {} did not finish within 30 s", h.id());
    }
    if let Some(e) = errs.lock().first() {
        return Err(e.clone());
    }
    let o = order.lock();
    kassert!(o.len() == 2 * USER_ROUNDS as usize, "{} rounds done", o.len());
    let changes = o.windows(2).filter(|w| w[0] != w[1]).count();
    kassert!(changes as u64 >= USER_ROUNDS, "the processes did not alternate ({changes} changes)");
    Ok(())
}

