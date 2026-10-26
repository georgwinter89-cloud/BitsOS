//! Host tests of the pure frame code: encode/decode round trips, selector and rflags rules, the stack
//! arithmetic of `place`, the FPU software-bytes plan and `check_image`'s refusals.

use super::*;
use alloc::vec;

const SHAPE: FpuShape = FpuShape { xfeatures: 0x7, user_size: 832 };

#[test]
fn frame_head_round_trips() {
    let mut h = FrameHead { restorer: 0x401000, uc_flags: UC_FP_XSTATE | UC_SIGCONTEXT_SS | UC_STRICT_RESTORE_SS, stack: [0; 24], mctx: Mcontext::default(), sigmask: 0xabcd };
    h.mctx.regs.rdi = 10;
    h.mctx.regs.rip = 0x400120;
    h.mctx.regs.rsp = 0x7fff_f000;
    h.mctx.regs.rflags = 0x202;
    h.mctx.cs = 0x33;
    h.mctx.ss = 0x2b;
    h.mctx.err = 4;
    h.mctx.trapno = 14;
    h.mctx.cr2 = 0x1234;
    h.mctx.fpstate = 0x7fff_e000;
    let b = encode(&h);
    let g = decode(&b);
    assert_eq!(g.restorer, h.restorer);
    assert_eq!(g.uc_flags, h.uc_flags);
    assert_eq!(g.sigmask, h.sigmask);
    assert_eq!(g.mctx.regs.rdi, 10);
    assert_eq!(g.mctx.regs.rip, 0x400120);
    assert_eq!(g.mctx.cs, 0x33);
    assert_eq!(g.mctx.ss, 0x2b);
    assert_eq!(g.mctx.fpstate, 0x7fff_e000);
    assert_eq!(g.mctx.cr2, 0x1234);
    // gs and fs words after cs/ss (offset 144..152) are always zero.
    assert_eq!(&b[48 + 146..48 + 150], &[0, 0, 0, 0]);
}

#[test]
fn rflags_and_selectors() {
    // Only FIX_EFLAGS bits come from the frame; IF (0x200) is kept from the current value.
    let restored = restored_rflags(0x202, 0xffff_ffff);
    assert_eq!(restored & 0x200, 0x200, "IF kept");
    assert_eq!(restored & FIX_EFLAGS, FIX_EFLAGS);
    assert_eq!(restored & 0x3000, 0, "IOPL never from the frame");
    assert!(selectors_ok(0x33, 0x2b, UC_STRICT_RESTORE_SS), "Linux user selectors");
    assert!(selectors_ok(crate::gdt::USER_CS, crate::gdt::USER_SS, UC_STRICT_RESTORE_SS), "V2's own");
    assert!(!selectors_ok(0x08, 0x2b, UC_STRICT_RESTORE_SS), "kernel CS refused");
    assert!(!selectors_ok(0x33, 0x10, UC_STRICT_RESTORE_SS), "kernel SS refused under strict");
    assert!(selectors_ok(0x33, 0x10, 0), "without strict, SS is not checked (forced valid)");
}

#[test]
fn place_default_and_altstack() {
    let sp = 0x7fff_f800;
    let alt = Alt { sp: 0x20_0000, size: 0x10000, autodisarm: false };
    // Without SA_ONSTACK: below sp, past the red zone, 64-byte fpstate, 16-aligned frame minus 8.
    let (frame, fp) = place(sp, false, alt, SHAPE.frame_bytes()).unwrap();
    assert!(frame < sp - REDZONE);
    assert_eq!(fp % 64, 0);
    assert_eq!((frame + 8) % 16, 0, "frame is 16-aligned minus 8");
    assert!(fp + SHAPE.user_size as u64 <= sp - REDZONE);
    // SA_ONSTACK and not on it: the frame is on the alternate stack.
    let (frame, _) = place(sp, true, alt, SHAPE.frame_bytes()).unwrap();
    assert!(frame > alt.sp && frame <= alt.sp + alt.size);
    // Already on it: stays on it (nested), no red zone off the alternate top matters.
    let (frame, _) = place(alt.sp + 0x8000, false, alt, SHAPE.frame_bytes()).unwrap();
    assert!(frame > alt.sp && frame < alt.sp + 0x8000);
    // An alternate stack too small: overflow -> None.
    let small = Alt { sp: 0x30_0000, size: 64, autodisarm: false };
    assert_eq!(place(sp, true, small, SHAPE.frame_bytes()), None);
}

#[test]
fn fpu_plan_from_sw() {
    // A well-formed sw area: full image.
    let mut sw = [0u8; 48];
    sw[0..4].copy_from_slice(&FP_XSTATE_MAGIC1.to_le_bytes());
    sw[4..8].copy_from_slice(&(SHAPE.user_size + 4).to_le_bytes());
    sw[8..16].copy_from_slice(&0x7u64.to_le_bytes());
    sw[16..20].copy_from_slice(&SHAPE.user_size.to_le_bytes());
    let p = plan_from_sw(&sw, SHAPE);
    assert!(!p.fx_only && p.xfeatures == 0x7 && p.magic2_at == Some(SHAPE.user_size as usize));
    assert_eq!(plan_with_magic2(p, FP_XSTATE_MAGIC2), p);
    assert!(plan_with_magic2(p, 0).fx_only, "a wrong magic2 falls back to the legacy area");
    // Bad magic1: legacy only.
    let mut bad = sw;
    bad[0] ^= 0xff;
    assert!(plan_from_sw(&bad, SHAPE).fx_only);
    // xstate_size too large: legacy only.
    let mut big = sw;
    big[16..20].copy_from_slice(&(SHAPE.user_size + 1000).to_le_bytes());
    assert!(plan_from_sw(&big, SHAPE).fx_only);
}

#[test]
fn check_image_refuses_hostile_headers() {
    let full = RestorePlan { fx_only: false, xfeatures: 0x7, magic2_at: Some(832) };
    let good = |f: &mut dyn FnMut(&mut [u8])| {
        let mut img = vec![0u8; SHAPE.user_size as usize];
        img[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
        img[XSTATE_BV_AT..XSTATE_BV_AT + 8].copy_from_slice(&0x7u64.to_le_bytes());
        f(&mut img);
        img
    };
    // A clean image: accepted, header rebuilt with the loaded states.
    let img = good(&mut |_| {});
    let out = check_image(&img, 0x4000, full, SHAPE, 0xFFFF).unwrap();
    assert_eq!(&out[XSTATE_BV_AT..XSTATE_BV_AT + 8], &0x7u64.to_le_bytes());
    // XSTATE_BV outside XCR0.
    let img = good(&mut |b| b[XSTATE_BV_AT] = 0xff);
    assert_eq!(check_image(&img, 0x4000, full, SHAPE, 0xFFFF), Err(BadFpu::StateBits));
    // XCOMP_BV != 0 (compacted).
    let img = good(&mut |b| b[XSTATE_BV_AT + 8] = 1);
    assert_eq!(check_image(&img, 0x4000, full, SHAPE, 0xFFFF), Err(BadFpu::Compacted));
    // A reserved header byte.
    let img = good(&mut |b| b[XSTATE_BV_AT + 16] = 1);
    assert_eq!(check_image(&img, 0x4000, full, SHAPE, 0xFFFF), Err(BadFpu::ReservedHeader));
    // MXCSR outside the mask.
    let img = good(&mut |b| b[27] = 0x80);
    assert_eq!(check_image(&img, 0x4000, full, SHAPE, 0xFFFF), Err(BadFpu::Mxcsr));
    // Misaligned (needs 64 for the full image).
    let img = good(&mut |_| {});
    assert_eq!(check_image(&img, 0x4010, full, SHAPE, 0xFFFF), Err(BadFpu::Misaligned));
    // fx_only: only the legacy area counts, 16-byte alignment, XSTATE_BV x87|SSE.
    let fx = RestorePlan { fx_only: true, xfeatures: XSTATE_FPSSE, magic2_at: None };
    let out = check_image(&img, 0x4010, fx, SHAPE, 0xFFFF).unwrap();
    assert_eq!(&out[XSTATE_BV_AT..XSTATE_BV_AT + 8], &3u64.to_le_bytes());
    assert_eq!(check_image(&img, 0x4008, fx, SHAPE, 0xFFFF), Err(BadFpu::Misaligned));
}

#[test]
fn fpu_exception_codes() {
    use super::exception_code;
    let mut legacy = vec![0u8; 512];
    // #MF: swd & ~cwd, invalid-operation bit set.
    legacy[0..2].copy_from_slice(&0x0000u16.to_le_bytes()); // cwd: exceptions unmasked
    legacy[2..4].copy_from_slice(&0x0001u16.to_le_bytes()); // swd: IE
    assert_eq!(exception_code(16, &legacy), 7, "FPE_FLTINV");
    legacy[2..4].copy_from_slice(&0x0004u16.to_le_bytes()); // ZE
    assert_eq!(exception_code(16, &legacy), 3, "FPE_FLTDIV");
    // #XM: ~(mxcsr>>7) & mxcsr. Divide-by-zero flag set, its mask clear.
    let mut l = vec![0u8; 512];
    let mxcsr: u32 = 0x1F80 & !0x200 | 0x004;
    l[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    assert_eq!(exception_code(19, &l), 3, "FPE_FLTDIV");
    // All masked: spurious, no signal.
    let mut l = vec![0u8; 512];
    l[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
    assert_eq!(exception_code(19, &l), 0);
}
