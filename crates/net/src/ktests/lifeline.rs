//! NW-4 ktests that only mean something while MCP runs (a normal boot with `token=`, the scenario
//! `host/tests/nw/50-lifeline.sh` calls them through MCP's `ktest` tool; on the Mini-PC the desk does). In a ktest
//! boot there is no MCP listener and they skip.

use super::{configured, st, SEC, SLIRP_GW};
use crate::{Ipv4, KSocket, NetError, SockAddr, TcpStream};
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};

/// The scenario's sink: a host server that takes and holds every connection (reached as 10.0.2.2).
const SINK: SockAddr = SockAddr { ip: SLIRP_GW, port: 24541 };
/// How many connections "the program" holds (V2-PLAN §6: 1,000).
const CONNS: usize = 1000;
const PLAIN_PORT: u16 = 24986;

/// Whether the lifeline's reserved listener (MCP) runs on the kernel's stack.
fn mcp_listens(s: &crate::stack::Stack) -> bool {
    s.kept_info().made > 0
}

/// V2-PLAN §6, the MCP reserve under pressure: 1,000 connections held (small buffers, as a program's), then every
/// socket buffer refused on every thread (the heap is gone as far as sockets go). A 1,001st connection and a new
/// listener get `NoMemory`; MCP must still accept 3 connections side by side and answer them: the host does 3
/// rounds of 3 while this test waits for 9 refills from the kept pool (`kept_info().reused`: one per accepted
/// connection, the spares and the sockets closed connections gave back), at most 120 s.
fn pressure() -> TestResult {
    let s = st()?;
    if configured(s)?.is_none() {
        return ktest::skip("no network card");
    }
    if !mcp_listens(s) {
        return ktest::skip("MCP does not listen (ktest boot): host/tests/nw/50-lifeline.sh calls this over MCP");
    }
    let first = match s.tcp_connect_sized(SINK, Some(3 * SEC), 4096, 4096) {
        Ok(c) => c,
        Err(_) => return ktest::skip("no sink on 10.0.2.2:24541 (host/tests/nw/50-lifeline.sh starts one)"),
    };
    let mut conns: Vec<Box<dyn TcpStream>> = Vec::new();
    conns.try_reserve(CONNS).map_err(|_| "no memory for the connection list")?;
    conns.push(first);
    while conns.len() < CONNS {
        let c = s.tcp_connect_sized(SINK, Some(10 * SEC), 4096, 4096).map_err(|x| format!("connection {}: {x:?}", conns.len() + 1))?;
        conns.push(c);
    }
    s.inject_no_memory(u32::MAX, None);
    let r = (|| -> TestResult {
        let more = s.tcp_connect_sized(SINK, Some(2 * SEC), 4096, 4096);
        kassert!(more.as_ref().err() == Some(&NetError::NoMemory), "connection 1,001 without memory: {:?}", more.as_ref().err());
        let l = s.tcp_listen(SockAddr { ip: Ipv4::ANY, port: PLAIN_PORT }, 2);
        kassert!(l.as_ref().err() == Some(&NetError::NoMemory), "a new listener without memory: {:?}", l.as_ref().err());
        let k0 = s.kept_info();
        klog::event!(Info, "net", "ktest.reserve_pressure_ready", conns = conns.len(), kept_made = k0.made, kept_idle = k0.idle;
            "socket memory refused on every thread; MCP lives on its kept sockets");
        let end = time::now_ns() + 120 * SEC;
        while s.kept_info().reused < k0.reused + 9 && time::now_ns() < end {
            sched::sleep_ns(SEC / 10);
        }
        let k = s.kept_info();
        klog::event!(Info, "net", "ktest.reserve_pressure", reused = k.reused - k0.reused, kept_made = k.made, kept_idle = k.idle);
        kassert!(k.reused >= k0.reused + 9, "only {} kept sockets came back in 120 s (the host makes 3 x 3 MCP calls)", k.reused - k0.reused);
        Ok(())
    })();
    s.inject_no_memory(0, None);
    drop(conns);
    r
}
ktest!("nw.reserve.pressure", QemuOnly, pressure);

/// k1-check (spec net.md §5.2): this CPU computes 20 s without ever yielding while the network keeps answering
/// from the others (ping, MCP on another connection; the host or the Mini-PC check list watches). Only over MCP.
fn busy_spin() -> TestResult {
    let s = st()?;
    if !mcp_listens(s) {
        return ktest::skip("MCP does not listen (ktest boot): run it over MCP while pinging and calling log");
    }
    let start = time::now_ns();
    let end = start + 20 * SEC;
    let mut n = 0u64;
    while time::now_ns() < end {
        n = n.wrapping_add(1);
        core::hint::spin_loop();
    }
    klog::event!(Info, "net", "ktest.busy_spin", ms = (time::now_ns() - start) / 1_000_000, loops = n);
    Ok(())
}
ktest!("nw.busy.spin", Safe, busy_spin);
