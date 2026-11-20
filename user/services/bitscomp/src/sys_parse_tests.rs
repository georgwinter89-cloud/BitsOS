//! SYS-A: the parsers of sys_parse against texts as Linux 7.3.1 (and V2's procfs) write them.

use super::*;

#[test]
fn proc_stat_meminfo_loadavg_uptime() {
    let s = stat("cpu  100 5 50 800 10 2 3 0 0 0\ncpu0 50 2 25 400 5 1 1 0 0 0\ncpu1 50 3 25 400 5 1 2 0 0 0\nintr 1 2\nctxt 123456\nbtime 1794584280\nprocesses 99\nprocs_running 3\n").unwrap();
    assert_eq!(s.all, CpuTimes { user: 100, nice: 5, system: 50, idle: 800, iowait: 10, irq: 2, softirq: 3, steal: 0 });
    assert_eq!((s.all.total(), s.all.programs(), s.all.kernel()), (970, 105, 55));
    assert_eq!(s.cpus.len(), 2);
    assert_eq!((s.ctxt, s.btime, s.running), (Some(123456), Some(1794584280), Some(3)));
    assert!(stat("intr 1\n").is_none(), "no cpu line: nothing");
    // An old kernel's four fields.
    assert_eq!(stat("cpu 1 2 3 4\n").unwrap().all.total(), 10);

    let m = meminfo("MemTotal:       24963392 kB\nMemFree:        19054592 kB\nMemAvailable:   21705728 kB\nBuffers:  102400 kB\nCached: 2097152 kB\nShmem: 307200 kB\nSReclaimable: 204800 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n").unwrap();
    assert_eq!(m.total, 24963392 * 1024);
    assert_eq!(m.available, 21705728 * 1024);
    assert_eq!(m.cache(), (102400 + 2097152 + 204800) * 1024);
    assert_eq!(m.used(), (24963392 - 19054592 - 102400 - 2097152 - 204800) * 1024);
    assert!(meminfo("Nothing: 1 kB\n").is_none());
    // Without MemAvailable (an old kernel): free + buffers + cache.
    assert_eq!(meminfo("MemTotal: 100 kB\nMemFree: 10 kB\nBuffers: 5 kB\nCached: 20 kB\n").unwrap().available, 35 * 1024);

    assert_eq!(loadavg("0.74 1.29 1.56 2/419 3301\n"), Some((0.74, 1.29, 1.56, 2, 419)));
    assert_eq!(loadavg("garbage"), None);
    assert_eq!(uptime("6840.25 98000.00\n"), Some(6840.25));
}

#[test]
fn a_process_stat_with_spaces_and_parentheses_in_its_name() {
    let line = "2817 (Game (x) 1) R 2790 2790 2790 0 -1 4194560 0 0 0 0 4100 300 0 0 20 -5 38 0 3817 7000000 602931 18446744073709551615\n";
    let p = pid_stat(line).unwrap();
    assert_eq!((p.pid, p.comm.as_str(), p.state, p.ppid, p.pgrp), (2817, "Game (x) 1", 'R', 2790, 2790));
    assert_eq!((p.utime, p.stime, p.nice, p.threads, p.start, p.rss_pages), (4100, 300, -5, 38, 3817, 602931));
    assert!(pid_stat("12 (x) R 1").is_none(), "a cut line is skipped");
    let st = pid_status("Name:\tx\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t2048 kB\nRssFile:\t100 kB\nRssShmem:\t28 kB\n").unwrap();
    assert_eq!(st, (1000, 2048 * 1024, 128 * 1024));
    assert!(pid_status("Name:\tx\n").is_none());
    assert_eq!(pid_io("rchar: 10\nwchar: 20\nread_bytes: 4096\nwrite_bytes: 8192\ncancelled_write_bytes: 0\n"), Some((4096, 8192)));
    assert_eq!(pid_io("rchar: 10\nwchar: 20\n"), Some((10, 20)));
    assert_eq!(pid_io(""), None);
}

#[test]
fn drm_fdinfo_counts_one_client_once() {
    let a = "pos:\t0\nflags:\t02\ndrm-driver:\tamdgpu\ndrm-client-id:\t7\ndrm-memory-vram:\t100 KiB\ndrm-memory-gtt:\t2 MiB\n";
    assert_eq!(drm_fdinfo(a), Some((7, 100 * 1024 + (2 << 20))));
    let b = "drm-client-id:\t8\ndrm-memory-vram:\t100 KiB\ndrm-resident-vram:\t1 GiB\ndrm-resident-gtt:\t512 KiB\n";
    assert_eq!(drm_fdinfo(b), Some((8, (1 << 30) + (512 << 10))), "resident sizes win over the old keys");
    assert_eq!(drm_fdinfo("pos:\t0\nflags:\t02\n"), None, "not a DRM file");
}

#[test]
fn net_disk_wireless_metrics() {
    let n = net_dev("Inter-|   Receive |  Transmit\n face |bytes packets ...\n    lo: 1000 10 0 0 0 0 0 0 1000 10 0 0 0 0 0 0\n  igc0: 53901 4000 0 0 0 0 0 0 1200 900 0 0 0 0 0 0\n");
    assert_eq!(n, vec![("lo".into(), 1000, 1000), ("igc0".into(), 53901, 1200)]);
    let d = diskstats(" 259 0 nvme0n1 120000 0 24000000 0 80000 0 16000000 0 0 50000 0\n 8 0 sda 1\n");
    assert_eq!(d, vec![("nvme0n1".into(), 24000000, 16000000, 50000)]);
    let w = wireless("Inter-| sta-|   Quality        |   Discarded packets\n face | tus | link level noise |  nwid  crypt\n wlan0: 0000   54.  -56.  -256        0      0\n");
    assert_eq!(w, vec![("wlan0".into(), 54.0, -56.0, -256.0)]);
    let m = metrics("cpu.all.busy 6.5 %\ncrash.since_boot 0 n\nbroken\n");
    assert_eq!(m, vec![("cpu.all.busy".into(), 6.5, "%".into()), ("crash.since_boot".into(), 0.0, "n".into())]);
}

#[test]
fn bitsos_files() {
    assert_eq!(generation("generation=151 reason=confirmed reload=0 version=0.1.0\n"), Some((151, "0.1.0".into())));
    assert_eq!(boot_ms("bitsos-boot 1\nstep kernel done 1200 \nstep desktop done 9800 x\nstep wlan failed 20000 \n"), Some(9800));
    assert_eq!(crash("new=0 source=disk slot=1 seq=7 wall=1794591120 boot=ab generation=150 cut=0\nnotice=x\n"), Some((1794591120, 150)));
    assert_eq!(crash("new=0 source=none decided=1\n"), None);
    assert_eq!(syscalls("   0 read                 calls=1000 errors=3\n   1 write  calls=500 errors=0 ENOSYS\nusercopy fast_path=1\n"), Some((1500, 3)));
    assert_eq!(syscalls(""), None);
    assert_eq!(cpu_model("processor\t: 0\nmodel name\t: AMD Ryzen 7 PRO 8845HS w/ Radeon 780M Graphics\ncpu cores\t: 8\n"), Some("AMD Ryzen 7 PRO 8845HS".into()));
    assert_eq!(cpu_model("model name\t: Intel(R) Core(TM) i5-8250U CPU @ 1.60GHz\n"), Some("Intel Core i5-8250U CPU @ 1.60GHz".into()));
    assert_eq!(cpu_cores("cpu cores\t: 8\n"), Some(8));
    assert_eq!(word("a=1 b=2", "b"), Some("2"));
}
