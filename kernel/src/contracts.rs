//! One ktest per G0 contract (V2-PLAN §9.1 point 4), against the stubs. When a lane replaces a stub, the
//! contract test must stay green; lanes add their own deeper tests next to their code.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use ktest::{kassert, ktest, TestResult};

fn frame_alloc() -> TestResult {
    let fa = mm_core::frames().ok_or("no frame allocator")?;
    let a = fa.alloc(0, mm_core::Zone::Normal, mm_core::Kind::Other).ok_or("alloc failed")?;
    let b = fa.alloc(4, mm_core::Zone::Dma32, mm_core::Kind::Dma).ok_or("dma32 alloc failed")?;
    kassert!(a % 4096 == 0 && b % (16 * 4096) == 0, "alignment");
    kassert!(b + 16 * 4096 <= 1 << 32, "DMA32 below 4 GiB");
    fa.free(a, 0, mm_core::Kind::Other);
    fa.free(b, 4, mm_core::Kind::Dma);
    Ok(())
}
ktest!("contract.frame_alloc", Safe, frame_alloc);

fn global_alloc() -> TestResult {
    let v: Vec<u64> = (0..100_000).collect();
    kassert!(v.iter().sum::<u64>() == 4_999_950_000);
    let big = alloc::vec![7u8; 3 << 20];
    kassert!(big[(3 << 20) - 1] == 7);
    Ok(())
}
ktest!("contract.global_alloc", Safe, global_alloc);

fn address_space() -> TestResult {
    let a = mm::AddressSpace::new().map_err(|e| alloc::format!("{e:?}"))?;
    a.map_fixed(0x40_0000, 3 * 4096, mm::Prot::rw(), mm::VmaKind::Anonymous).map_err(|e| alloc::format!("{e:?}"))?;
    a.copy_to_user(0x40_1ff8, b"crosses!page").map_err(|e| alloc::format!("{e:?}"))?;
    let mut back = [0u8; 12];
    a.copy_from_user(&mut back, 0x40_1ff8).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(&back == b"crosses!page");
    a.protect(0x40_0000, 4096, mm::Prot { read: true, write: false, exec: false }).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(a.copy_to_user(0x40_0000, b"x").is_err(), "write to read-only must fail");
    kassert!(a.copy_from_user(&mut back, 0x90_0000).is_err(), "unmapped read must fail");
    kassert!(a.copy_to_user(hal::USER_TOP - 2, b"xyz").is_err(), "past USER_TOP must fail");
    let m = a.map_anywhere(10_000, mm::Prot::rw()).map_err(|e| alloc::format!("{e:?}"))?;
    a.unmap(m, 10_000).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(a.copy_from_user(&mut back, m).is_err(), "unmapped after munmap");
    Ok(())
}
ktest!("contract.address_space", Safe, address_space);

fn dma_pool() -> TestResult {
    let b = mm::dma::DmaBuffer::alloc(10_000, mm::dma::DmaConstraints::default()).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(b.device_addr() + 16384 <= 1 << 32);
    kassert!(b.write_u32(4, 0xDEAD_BEEF));
    kassert!(b.read_u32(4) == Some(0xDEAD_BEEF));
    kassert!(!b.write(9_999, &[1, 2]), "out of bounds write refused");
    Ok(())
}
ktest!("contract.dma_pool", Safe, dma_pool);

fn threads_and_waits() -> TestResult {
    static C: ksync::Completion = ksync::Completion::new();
    static N: AtomicUsize = AtomicUsize::new(0);
    // Statics: the MCP ktest tool may run this again in the same boot.
    C.reset();
    N.store(0, Ordering::SeqCst);
    let m = Arc::new(ksync::Mutex::named("ktest", 0u32));
    let mut hs = Vec::new();
    for i in 0..4 {
        let m = m.clone();
        let class = if i == 0 { sched::Class::Interactive } else { sched::Class::Normal };
        hs.push(sched::spawn("ktest-worker", class, move || {
            for _ in 0..100 {
                *m.lock() += 1;
                sched::yield_now();
            }
            if N.fetch_add(1, Ordering::AcqRel) == 3 {
                C.complete();
            }
        }).map_err(|e| alloc::format!("{e:?}"))?);
    }
    kassert!(C.wait_timeout(5_000_000_000), "workers did not finish");
    for h in &hs {
        h.join();
    }
    kassert!(*m.lock() == 400);
    sched::reap();
    Ok(())
}
ktest!("contract.threads_waitqueue_mutex", Safe, threads_and_waits);

fn hrtimer() -> TestResult {
    static FIRED: AtomicU32 = AtomicU32::new(0);
    fn cb(a: usize) {
        FIRED.fetch_add(a as u32, Ordering::SeqCst);
    }
    FIRED.store(0, Ordering::SeqCst);
    let t = time::HrTimer::new(cb, 1);
    t.arm_in(2_000_000);
    let t2 = time::HrTimer::new(cb, 100);
    t2.arm_in(1_000_000_000);
    t2.cancel();
    sched::sleep_ms(20);
    time::poll_timers();
    kassert!(FIRED.load(Ordering::SeqCst) == 1, "fired {}", FIRED.load(Ordering::SeqCst));
    let a = time::now_ns();
    sched::sleep_ms(10);
    kassert!(time::now_ns() - a >= 10_000_000, "sleep too short");
    Ok(())
}
ktest!("contract.hrtimer_sleep", Safe, hrtimer);

fn percpu() -> TestResult {
    static P: hal::percpu::PerCpu<AtomicU32> = hal::percpu::PerCpu::new(|i| AtomicU32::new(i as u32 + 7));
    kassert!(P.get().load(Ordering::Relaxed) == 7);
    kassert!(P.iter().count() == hal::percpu::cpu_count());
    Ok(())
}
ktest!("contract.percpu", Safe, percpu);

fn irq_vector() -> TestResult {
    fn h(_v: u8) {}
    let v = hal::vectors::alloc(h, None).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!((hal::vectors::FIRST_DYNAMIC..=hal::vectors::LAST_DYNAMIC).contains(&v.vector));
    let w = hal::vectors::alloc(h, None).map_err(|e| alloc::format!("{e:?}"))?;
    // With several CPUs the same vector number is legal on two CPUs: the (cpu, vector) pairs differ.
    kassert!(v != w, "two allocations got the same (cpu, vector): {v:?}");
    hal::vectors::free(v);
    hal::vectors::free(w);
    Ok(())
}
ktest!("contract.irq_vector", Safe, irq_vector);

struct Q(AtomicU32);
impl devmodel::Quiesce for Q {
    fn name(&self) -> &str {
        "ktest-quiesce"
    }
    fn quiesce(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
fn quiesce() -> TestResult {
    // Registered once per boot: the MCP ktest tool may run this test again, and registrations stay.
    static Q1: ksync::Once<Arc<Q>> = ksync::Once::new();
    let q = Q1.get_or_init(|| {
        let q = Arc::new(Q(AtomicU32::new(0)));
        devmodel::register_quiesce(q.clone());
        q
    });
    let before = q.0.load(Ordering::SeqCst);
    // Only the test device: quiesce_all would stop every real driver for the later ktests.
    kassert!(devmodel::quiesce_matching("ktest-quiesce") == 1);
    kassert!(q.0.load(Ordering::SeqCst) == before + 1);
    Ok(())
}
ktest!("contract.quiesce", Safe, quiesce);

// PCI: the scan found the q35 host bridge, and an opt-in driver is skipped without its key.
static PROBED: AtomicU32 = AtomicU32::new(0);
fn optin_probe(_f: &pci::PciFunction) -> Result<(), alloc::string::String> {
    PROBED.fetch_add(1, Ordering::SeqCst);
    Ok(())
}
pci::pci_driver!(pci::DriverEntry {
    name: "ktest-optin",
    // Only q35's MCH, so real machines never mark their host bridges untouched.
    matches: &[pci::Match::Id { vendor: 0x8086, device: 0x29C0 }],
    opt_in_key: Some("ktest_optin"),
    probe: optin_probe,
});
fn pci_registry() -> TestResult {
    let devs = pci::devices();
    kassert!(!devs.is_empty(), "no PCI function found");
    kassert!(devs.iter().any(|d| d.class >> 8 == 0x0600), "no host bridge");
    kassert!(PROBED.load(Ordering::SeqCst) == 0, "opt-in driver attached without its key");
    Ok(())
}
ktest!("contract.pci_registry_optin", Safe, pci_registry);

fn ksocket() -> TestResult {
    let s = net::stack().ok_or("no stack")?;
    let any = net::SockAddr { ip: net::Ipv4::ANY, port: 24999 };
    let l = s.tcp_listen(any, 3).map_err(|e| alloc::format!("{e:?}"))?;
    let c = s.tcp_connect(net::SockAddr { ip: net::Ipv4::LOCALHOST, port: 24999 }, Some(1_000_000_000)).map_err(|e| alloc::format!("{e:?}"))?;
    let a = l.accept(Some(1_000_000_000)).map_err(|e| alloc::format!("{e:?}"))?;
    c.write_all(b"POST /mcp", None).map_err(|e| alloc::format!("{e:?}"))?;
    let mut b = [0u8; 16];
    let n = a.read(&mut b, Some(1_000_000_000)).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(&b[..n] == b"POST /mcp");
    Ok(())
}
ktest!("contract.ksocket_loopback", Safe, ksocket);

fn netdevice() -> TestResult {
    use devmodel::net::NetDevice;
    let d = net::loopback::LoopDevice::new("lo-test");
    d.transmit(&[0xAA; 60]).map_err(|e| alloc::format!("{e:?}"))?;
    let mut buf = [0u8; 2048];
    kassert!(d.receive(&mut buf) == Some(60));
    kassert!(d.poll_tx(&[1; 20]), "emergency path");
    Ok(())
}
ktest!("contract.netdevice", Safe, netdevice);

fn pollable() -> TestResult {
    let ns = vfs::root_ns();
    let n = ns.lookup("/dev/console").map_err(|e| alloc::format!("errno {e}"))?;
    let f = vfs::node::NodeFile::new(n);
    let ev = vfs::poll::wait_for(&f, vfs::Events::OUT, Some(1_000_000));
    kassert!(ev.contains(vfs::Events::OUT));
    Ok(())
}
ktest!("contract.pollable", Safe, pollable);

fn console_ramframe() -> TestResult {
    use console::Console;
    if display::device().is_none() {
        return Ok(());
    }
    console::console().write_str("contract.console_ramframe\n", console::FG);
    let (w, h, px) = display::snapshot().ok_or("no frame")?;
    kassert!(w > 0 && h > 0 && px.len() == (w * h) as usize);
    kassert!(px.iter().any(|&p| p == console::FG), "text pixels in the RAM frame");
    Ok(())
}
ktest!("contract.console_ramframe", Safe, console_ramframe);

fn evdev() -> TestResult {
    // IN-2a CR1 / IN-3 CR3: real HID devices feed the sink too, so drain first and find the injected press; then
    // release the key again (the held-key union and the VT's client must not keep a stuck "a").
    let s = console::evdev::sink();
    while s.pop().is_some() {}
    s.push(console::evdev::InputEvent { time_ns: time::now_ns(), kind: 1, code: 30, value: 1, device: 0, injected: true });
    let e = core::iter::from_fn(|| s.pop()).find(|e| e.injected && e.code == 30);
    s.push(console::evdev::InputEvent { time_ns: time::now_ns(), kind: 1, code: 30, value: 0, device: 0, injected: true });
    while s.pop().is_some() {}
    let e = e.ok_or("event lost")?;
    kassert!(e.injected && e.code == 30 && e.value == 1);
    Ok(())
}
ktest!("contract.evdev_injected", Safe, evdev);

fn events() -> TestResult {
    let start = klog::next_seq();
    klog::event!(Info, "ktest", "event.check", answer = 42u32, name = "x y");
    let r = klog::read_since(start, 10);
    let line = r.iter().find(|r| r.name == "event.check").ok_or("event not stored")?.render();
    kassert!(line.contains("I ktest event.check answer=42 name=\"x y\""), "{}", line);
    Ok(())
}
ktest!("contract.b6_events", Safe, events);

/// RB-7b CR4: the `metrics` tool exists (owner RB, Read) and the sampler fills the base series within 3 periods.
fn metrics() -> TestResult {
    let t = mcp::find("metrics").ok_or("metrics tool missing")?;
    kassert!(t.owner == "RB" && t.safety == mcp::Safety::Read);
    let until = time::now_ns() + 3 * klogd::metrics::PERIOD_NS;
    loop {
        let names = klogd::metrics::with_store(1_000_000_000, |s| ["cpu.all.busy", "metrics.cost", "mem.total"].iter().all(|n| s.has(n)));
        match names {
            None => return ktest::skip("metrics=off"),
            Some(true) => return Ok(()),
            Some(false) if time::now_ns() < until => sched::sleep_ms(200),
            Some(false) => return Err("cpu.all.busy/metrics.cost/mem.total missing after 3 periods".into()),
        }
    }
}
ktest!("contract.b6_metrics", Safe, metrics);

fn policy_hook() -> TestResult {
    let s = policy::Subject { program: policy::ProgramId(77), pid: 5, uid: 1000 };
    let before = policy::counts()[1].1;
    kassert!(policy::check(&s, &policy::Access::NetConnect { proto: policy::NetProto::Tcp, addr: policy::IpAddr::V4([10, 0, 0, 1]), port: 80 }).is_ok());
    kassert!(policy::counts()[1].1 == before + 1, "net access counted");
    Ok(())
}
ktest!("contract.b3_policy_hook", Safe, policy_hook);

fn mount_ns() -> TestResult {
    let a = vfs::root_ns();
    let b = vfs::root_ns();
    kassert!(Arc::ptr_eq(&a, &b), "G0: one shared view");
    kassert!(a.lookup("/dev/console").is_ok());
    Ok(())
}
ktest!("contract.b2_mount_ns", Safe, mount_ns);

struct NoFreeze;
impl vfs::FsFreeze for NoFreeze {
    fn freeze(&self) -> Result<(), vfs::Errno> {
        Ok(())
    }
    fn thaw(&self) {}
}
fn freeze() -> TestResult {
    let f: &dyn vfs::FsFreeze = &NoFreeze;
    kassert!(f.freeze().is_ok());
    f.thaw();
    Ok(())
}
ktest!("contract.b1_fs_freeze", Safe, freeze);

fn mcp_registry() -> TestResult {
    let t = mcp::find("version").ok_or("version tool missing")?;
    let ctx = mcp::ToolContext { caller: policy::Subject::KERNEL, deadline_ns: time::now_ns() + 1_000_000_000 };
    let out = (t.handler)(&ctx, &mcp::Json::Null).map_err(|e| alloc::format!("{e:?}"))?;
    kassert!(matches!(&out[0], mcp::ToolOutput::Text(s) if s.starts_with("BitsOS V2")));
    kassert!(t.safety == mcp::Safety::Read && t.owner == "MC");
    Ok(())
}
ktest!("contract.b5_mcp_tool", Safe, mcp_registry);

fn registries_present() -> TestResult {
    kassert!(syscall::SYSCALLS.len() >= 20, "wave-A stubs registered");
    // INT-A1: FS-2's devfs makes its own nodes (null, zero, console, ...) and the console plugs in through
    // `vfs::tty::set_console`, so no crate needs `devnode!` today; the registry stays for driver nodes.
    let _ = vfs::DEVNODES.len();
    for n in ["/dev/null", "/dev/console", "/dev/tty"] {
        kassert!(vfs::root_ns().open(n).is_ok(), "{n} missing in devfs");
    }
    kassert!(kreg::INITCALLS.len() >= 1);
    kassert!(ktest::KTESTS.len() >= 20);
    Ok(())
}
ktest!("kreg.registries_present", Safe, registries_present);
