//! Host tests of the formats (the files themselves are checked by the `ab7.*` ktests and `abi-procfs`).

use super::fd::fdinfo_head;
use super::maps::{image_lines, line, share_of};
use mm::PageUsage;
use super::mounts::{Row, format_rows};
use super::stat::{bitmap_hex, bitmap_list, sigset};
use super::text::{badness_points, format_limits, parse_int};
use alloc::string::String;
use alloc::vec;
use proc::procinfo::{ImageInfo, ImageSeg};
use uapi::ab::rlimit::RLIM_INFINITY;
use uapi::errno::*;

#[test]
fn cpu_and_node_masks_as_percent_pb() {
    assert_eq!(bitmap_hex(&[0xffff], 16), "ffff");
    assert_eq!(bitmap_hex(&[0xfffff], 20), "fffff");
    assert_eq!(bitmap_hex(&[0x1_0000_0001], 40), "01,00000001");
    let nodes = bitmap_hex(&[1], 1024);
    assert_eq!(nodes.len(), 32 * 9 - 1);
    assert!(nodes.starts_with("00000000,") && nodes.ends_with(",00000001"));
    assert_eq!(bitmap_list(&[0xffff], 16), "0-15");
    assert_eq!(bitmap_list(&[0b1011_0001], 16), "0,4-5,7");
    assert_eq!(bitmap_list(&[0], 16), "");
    assert_eq!(sigset(1 << 9), "0000000000000200");
}

#[test]
fn limits_columns_like_proc_pid_limits() {
    let mut l = vec![(RLIM_INFINITY, RLIM_INFINITY); 16];
    l[7] = (1024, 524288);
    l[13] = (0, 0);
    let s = format_limits(&l);
    let lines: alloc::vec::Vec<&str> = s.lines().collect();
    assert_eq!(lines.len(), 17);
    assert_eq!(lines[0], "Limit                     Soft Limit           Hard Limit           Units     ");
    assert_eq!(lines[1], "Max cpu time              unlimited            unlimited            seconds   ");
    assert_eq!(lines[8], "Max open files            1024                 524288               files     ");
    assert_eq!(lines[14], "Max nice priority         0                    0                    ");
    assert_eq!(lines[16], "Max realtime timeout      unlimited            unlimited            us        ");
}

#[test]
fn kstrtoint_rules() {
    assert_eq!(parse_int(b"500\n"), Ok(500));
    assert_eq!(parse_int(b" -1000 "), Ok(-1000));
    assert_eq!(parse_int(b"+7"), Ok(7));
    assert_eq!(parse_int(b"0x10"), Ok(16));
    assert_eq!(parse_int(b"010"), Ok(8));
    assert_eq!(parse_int(b"0"), Ok(0));
    assert_eq!(parse_int(b""), Err(EINVAL));
    assert_eq!(parse_int(b"12a"), Err(EINVAL));
    assert_eq!(parse_int(b"08"), Err(EINVAL));
    assert_eq!(parse_int(b"99999999999"), Err(ERANGE));
    // A NUL ends the text as in the C buffer Linux parses; what follows it is not looked at.
    assert_eq!(parse_int(b"5\0xyz"), Ok(5));
    assert_eq!(parse_int(b"\0"), Err(EINVAL));
    assert_eq!(parse_int(b"0x"), Err(EINVAL));
    assert_eq!(parse_int(b"-2147483648"), Ok(i32::MIN));
}

#[test]
fn oom_score_scale() {
    // No memory used and no adjustment: 666 (Linux's (1000 + 0) * 2 / 3).
    assert_eq!(badness_points(0, 0, 1_000_000), 666);
    assert_eq!(badness_points(0, 1000, 1_000_000), 1333);
    assert_eq!(badness_points(500_000, 0, 1_000_000), 1000);
}

#[test]
fn maps_line_shape() {
    let mut s = String::new();
    line(&mut s, 0x7ffd_0000_0000, 0x7ffd_0002_1000, *b"rw-p", 0, 0, 0, Some("[stack]"));
    line(&mut s, 0x1000, 0x2000, *b"r--p", 0, 0, 0, None);
    let l: alloc::vec::Vec<&str> = s.lines().collect();
    assert_eq!(&l[0][..48], "7ffd00000000-7ffd00021000 rw-p 00000000 00:00 0 ");
    assert_eq!(l[0].find("[stack]"), Some(73));
    assert_eq!(l[1], "00001000-00002000 r--p 00000000 00:00 0 ");
}

#[test]
fn image_vma_split_into_file_and_bss() {
    let info = ImageInfo { segs: vec![ImageSeg { start: 0x1000, file_end: 0x3000, end: 0x5000, off: 0x2000 }], ..Default::default() };
    let mut s = String::new();
    image_lines(&mut s, 0x1000, 0x5000, *b"rw-p", &info, (0x803, 42, "/bin/x"), &mut |_, _, _| {});
    let l: alloc::vec::Vec<&str> = s.lines().collect();
    assert_eq!(l.len(), 2, "{s}");
    assert!(l[0].starts_with("00001000-00003000 rw-p 00002000 08:03 42 ") && l[0].ends_with(" /bin/x"), "{s}");
    assert_eq!(l[1], "00003000-00005000 rw-p 00000000 00:00 0 ");
    // A VMA that starts inside the segment (an mprotect split) keeps the file offset of its start.
    let mut s = String::new();
    image_lines(&mut s, 0x2000, 0x3000, *b"r--p", &info, (0x803, 42, "/bin/x"), &mut |_, _, _| {});
    assert!(s.starts_with("00002000-00003000 r--p 00003000 08:03 42 "), "{s}");
}

#[test]
fn image_vma_usage_split_sums_exactly() {
    // 5 resident pages over the lines [0x1000,0x3000) (file part, 2 pages) and [0x3000,0x6000) (.bss, 3 pages).
    let u = PageUsage { resident: 5 * 4096, pss: (5 * 4096) << 12, private_dirty: 5 * 4096, referenced: 3 * 4096, anonymous: 5 * 4096, ..Default::default() };
    let (mut taken, mut given) = (0, PageUsage::default());
    let a = share_of(&u, 0x1000, 0x3000, false, &mut taken, &mut given);
    let b = share_of(&u, 0x3000, 0x6000, true, &mut taken, &mut given);
    assert_eq!((a.resident, b.resident), (2 * 4096, 3 * 4096));
    let mut sum = a;
    sum.add(&b);
    assert_eq!(sum, u, "the lines sum to the VMA");
    // Fewer resident pages than the file part: all of them go to the first line.
    let u = PageUsage { resident: 4096, anonymous: 4096, ..Default::default() };
    let (mut taken, mut given) = (0, PageUsage::default());
    let a = share_of(&u, 0x1000, 0x3000, false, &mut taken, &mut given);
    let b = share_of(&u, 0x3000, 0x6000, true, &mut taken, &mut given);
    assert_eq!((a.resident, a.anonymous, b.resident, b.anonymous), (4096, 4096, 0, 0));
    // The callback sees every line in order.
    let info = ImageInfo { segs: vec![ImageSeg { start: 0x1000, file_end: 0x3000, end: 0x6000, off: 0 }], ..Default::default() };
    let mut seen = alloc::vec::Vec::new();
    image_lines(&mut String::new(), 0x1000, 0x6000, *b"rw-p", &info, (1, 2, "/x"), &mut |_, a, b| seen.push((a, b)));
    assert_eq!(seen, [(0x1000, 0x3000), (0x3000, 0x6000)]);
}

#[test]
fn image_vma_usage_split_degenerate() {
    let split = |u: &PageUsage, lines: &[(u64, u64)]| {
        let (mut taken, mut given) = (0, PageUsage::default());
        let n = lines.len();
        lines.iter().enumerate().map(|(i, &(a, b))| share_of(u, a, b, i + 1 == n, &mut taken, &mut given)).collect::<alloc::vec::Vec<_>>()
    };
    let sum = |parts: &[PageUsage]| parts.iter().fold(PageUsage::default(), |mut s, p| {
        s.add(p);
        s
    });
    // All-zero usage: every line is zero.
    let z = split(&PageUsage::default(), &[(0x1000, 0x3000), (0x3000, 0x6000)]);
    assert!(z.iter().all(|p| *p == PageUsage::default()));
    // A single line is the last one and gets the whole VMA.
    let u = PageUsage { resident: 3 * 4096, pss: (3 * 4096) << 12, private_clean: 3 * 4096, ..Default::default() };
    assert_eq!(split(&u, &[(0x1000, 0x4000)]), [u]);
    // A zero-length line and a reversed one get nothing and do not underflow; the sum stays exact.
    let parts = split(&u, &[(0x1000, 0x1000), (0x3000, 0x2000), (0x1000, 0x4000)]);
    assert_eq!((parts[0], parts[1]), (PageUsage::default(), PageUsage::default()));
    assert_eq!(sum(&parts), u);
    // No resident pages but other numbers (cannot happen from MM; still no division by zero): the last
    // line takes them all.
    let odd = PageUsage { referenced: 4096, pss: 7, ..Default::default() };
    let parts = split(&odd, &[(0x1000, 0x3000), (0x3000, 0x6000)]);
    assert_eq!((parts[0], parts[1]), (PageUsage::default(), odd));
}

#[test]
fn mount_lines() {
    let pseudo = vfs::MNT_NOSUID | vfs::MNT_NODEV | vfs::MNT_NOEXEC | vfs::MNT_RELATIME;
    let rows = [
        Row { id: 1, parent: 1, dev: 0x10, point: b"/".to_vec(), fstype: "tmpfs".into(), source: "tmpfs".into(), readonly: false, flags: vfs::MNT_RELATIME, options: ",mode=755,inode64".into() },
        Row { id: 5, parent: 1, dev: 0x803, point: b"/mnt/a b#".to_vec(), fstype: "vfat".into(), source: "/dev/x#1".into(), readonly: true, flags: vfs::MNT_RELATIME, options: ",fmask=0022".into() },
        Row { id: 6, parent: 1, dev: 0x11, point: b"/proc".to_vec(), fstype: "proc".into(), source: "proc".into(), readonly: false, flags: pseudo, options: String::new() },
        Row { id: 7, parent: 1, dev: 0x12, point: b"/dev".to_vec(), fstype: "devtmpfs".into(), source: "devtmpfs".into(), readonly: false, flags: vfs::MNT_NOSUID, options: ",mode=755,inode64".into() },
    ];
    assert_eq!(
        format_rows(&rows, false),
        "tmpfs / tmpfs rw,relatime,mode=755,inode64 0 0\n/dev/x\\0431 /mnt/a\\040b# vfat ro,relatime,fmask=0022 0 0\n\
         proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0\ndevtmpfs /dev devtmpfs rw,nosuid,mode=755,inode64 0 0\n"
    );
    assert_eq!(
        format_rows(&rows, true),
        "1 1 0:16 / / rw,relatime - tmpfs tmpfs rw,mode=755,inode64\n5 1 8:3 / /mnt/a\\040b# ro,relatime - vfat /dev/x\\0431 ro,fmask=0022\n\
         6 1 0:17 / /proc rw,nosuid,nodev,noexec,relatime - proc proc rw\n7 1 0:18 / /dev rw,nosuid - devtmpfs devtmpfs rw,mode=755,inode64\n"
    );
    // show_mnt_opts' last entry (linux-6.6/fs/proc_namespace.c:73).
    let r = [Row { id: 2, parent: 1, dev: 0x13, point: b"/s".to_vec(), fstype: "tmpfs".into(), source: "tmpfs".into(), readonly: false, flags: vfs::MNT_RELATIME | vfs::MNT_NOSYMFOLLOW, options: String::new() }];
    assert_eq!(format_rows(&r, false), "tmpfs /s tmpfs rw,relatime,nosymfollow 0 0\n");
}

#[test]
fn fdinfo_head_lines() {
    assert_eq!(fdinfo_head(5, 0o2100002, 3, 7), "pos:\t5\nflags:\t02100002\nmnt_id:\t3\nino:\t7\n");
}

// AB-7b: the system-wide files.

use super::cpuinfo::{Common, Cpu, block, package_counts};
use super::sys_cpu::words;
use super::sys_net::{mac_text, operstate_name};
use super::sysctl::{uts_write, uuid_text};
use super::system::{CpuTimes, Mem, StatTail, loadavg, meminfo, stat, uptime};

#[test]
fn meminfo_lines_as_show_val_kb() {
    let m = meminfo(&Mem { total: 32_000_000, free: 30_000_000, shmem: 1024, file: 2048, commit_limit: 31_000_000, committed: 123, available: 30_000_000 });
    let lines: alloc::vec::Vec<&str> = m.lines().collect();
    assert_eq!(lines[0], "MemTotal:       32000000 kB");
    assert_eq!(lines[2], "MemAvailable:   30000000 kB");
    assert!(lines.contains(&"Committed_AS:        123 kB"));
    assert!(lines.contains(&"CommitLimit:    31000000 kB"));
    assert!(lines.contains(&"Shmem:              1024 kB"));
    assert!(lines.contains(&"Cached:             3072 kB") && lines.contains(&"Active:             3072 kB"));
    assert!(lines.contains(&"Active(file):       2048 kB") && lines.contains(&"Inactive(file):        0 kB"));
    assert!(lines.contains(&"HardwareCorrupted:     0 kB"));
    assert!(lines.contains(&"HugePages_Total:       0"));
    assert!(lines.contains(&"Hugepagesize:       2048 kB"));
    assert_eq!(lines.last(), Some(&"Hugetlb:               0 kB"));
    // Every kB line is the 16-column key plus an 8-wide number (for numbers below 10^8).
    for l in lines.iter().filter(|l| l.ends_with(" kB") && !l.starts_with("HardwareCorrupted") && !l.starts_with("Vmalloc")) {
        assert_eq!(l.len(), 27, "{l}");
    }
    let keys: alloc::vec::Vec<&str> = lines.iter().map(|l| l.split(':').next().unwrap()).collect();
    let pos = |k: &str| keys.iter().position(|x| *x == k).unwrap();
    assert!(pos("SwapFree") < pos("Zswap") && pos("Shmem") < pos("KReclaimable") && pos("CommitLimit") < pos("Committed_AS"));
    assert!(pos("FileHugePages") < pos("FilePmdMapped") && pos("Unaccepted") < pos("HugePages_Total"));
}

#[test]
fn stat_cpu_lines_in_user_hz() {
    let c = CpuTimes { user: 2_000_000_000, nice: 10_000_000, system: 30_000_000, idle: 5_000_000_000 };
    let s = stat(&[(0, c), (1, c)], &StatTail { ctxt: 7, btime: 1_700_000_000, processes: 42, running: 3 });
    let l: alloc::vec::Vec<&str> = s.lines().collect();
    assert_eq!(l[0], "cpu  400 2 6 1000 0 0 0 0 0 0");
    assert_eq!(l[1], "cpu0 200 1 3 500 0 0 0 0 0 0");
    assert_eq!(l[2], "cpu1 200 1 3 500 0 0 0 0 0 0");
    assert_eq!(&l[3..], ["intr 0", "ctxt 7", "btime 1700000000", "processes 42", "procs_running 3", "procs_blocked 0", "softirq 0 0 0 0 0 0 0 0 0 0 0"]);
}

#[test]
fn uptime_and_loadavg_formats() {
    assert_eq!(uptime(12_345_678_901, 99_990_000_000), "12.34 99.99\n");
    assert_eq!(uptime(0, 0), "0.00 0.00\n");
    // 1.0 in FSHIFT 11 is 2048; get_avenrun adds FIXED_1/200 (10) before cutting to two decimals.
    assert_eq!(loadavg([2048, 1024, 0], 2, 150, 4321), "1.00 0.50 0.00 2/150 4321\n");
    assert_eq!(loadavg([2047, 3 * 2048 + 2030, 20], 1, 1, 99), "1.00 3.99 0.01 1/1 99\n");
}

#[test]
fn interrupts_format() {
    // Linux 6.6 on a 2-CPU machine (prec 3): 11 blanks, CPUn left-aligned in 8, rows "%*s: " + "%10u ".
    let want = "           CPU0       CPU1       \nRES:          5 4294967295   Rescheduling interrupts\nTLB:          0          7   TLB shootdowns\n";
    assert_eq!(super::system::interrupts(&[(0, 5, 0), (1, u64::from(u32::MAX), 7)]), want);
}

#[test]
fn uuid_hostname_and_net_texts() {
    let u = uuid_text([0xff; 16]);
    assert_eq!(u, "ffffffff-ffff-4fff-bfff-ffffffffffff\n");
    assert_eq!(uuid_text([0; 16]), "00000000-0000-4000-8000-000000000000\n");
    assert_eq!(uts_write(b"box\n"), b"box");
    assert_eq!(uts_write(b"a\0b"), b"a");
    assert_eq!(uts_write(&[b'x'; 80]).len(), 64);
    assert_eq!(mac_text(&[0, 0x1b, 0x21, 0xa, 0xbc, 0xff]), "00:1b:21:0a:bc:ff\n");
    assert_eq!(operstate_name(0), "unknown");
    assert_eq!(operstate_name(2), "down");
    assert_eq!(operstate_name(6), "up");
    assert_eq!(words([0usize, 3, 64, 200].into_iter(), 70), alloc::vec![0b1001, 1]);
}

#[test]
fn cpuinfo_block_with_smt_topology() {
    // The 8845HS: one package, 8 cores, 2 threads each (APIC IDs 0..15, core = apic / 2).
    let all: alloc::vec::Vec<Cpu> = (0..16).map(|i| Cpu { cpu: i, pkg: 0, die: 0, cluster: 0, core: i as u32 / 2, apic: i as u32 }).collect();
    assert_eq!(package_counts(&all, &all[5]), (16, 8));
    let m = Common {
        vendor: String::from("AuthenticAMD"),
        family: 25,
        model: 117,
        brand: String::from("AMD Ryzen 7 8845HS w/ Radeon 780M Graphics"),
        stepping: 2,
        tsc_hz: 3_793_000_000,
        cache_kb: 1024,
        level: 16,
        flags: String::from("fpu vme avx2"),
        tlb: 3584,
        clflush: 64,
        phys: 48,
        virt: 48,
        power: (1 << 0) | (1 << 8) | (1 << 9) | (1 << 20),
    };
    let mut s = String::new();
    block(&mut s, &m, &all[5], 16, 8);
    assert!(s.starts_with("processor\t: 5\nvendor_id\t: AuthenticAMD\ncpu family\t: 25\nmodel\t\t: 117\n"));
    for want in [
        "cpu MHz\t\t: 3793.000\n",
        "cache size\t: 1024 KB\n",
        "physical id\t: 0\nsiblings\t: 16\ncore id\t\t: 2\ncpu cores\t: 8\napicid\t\t: 5\ninitial apicid\t: 5\n",
        "flags\t\t: fpu vme avx2\nbugs\t\t:\nbogomips\t: 7586.00\nTLB size\t: 3584 4K pages\n",
        "address sizes\t: 48 bits physical, 48 bits virtual\npower management: ts cpb [20]\n\n",
    ] {
        assert!(s.contains(want), "{want:?} missing in {s}");
    }
}

#[test]
fn cpu_lists_for_odd_and_tiny_counts() {
    use super::sys_cpu::list_line;
    assert_eq!(list_line(0..1, 1), "0\n");
    assert_eq!(list_line(0..2, 2), "0-1\n");
    assert_eq!(list_line(0..17, 17), "0-16\n");
    assert_eq!(list_line(0..0, 1), "\n");
    assert_eq!(list_line([1usize, 3, 4, 5, 16].into_iter(), 17), "1,3-5,16\n");
    assert_eq!(list_line(0..130, 130), "0-129\n");
}

#[test]
fn cpuinfo_without_topology_and_with_one_cpu() {
    use super::cpuinfo::cpu_from;
    let c = cpu_from(7, None);
    assert_eq!((c.cpu, c.pkg, c.core, c.apic), (7, 0, 7, 7));
    let one = [cpu_from(0, None)];
    assert_eq!(package_counts(&one, &one[0]), (1, 1));
    let odd: alloc::vec::Vec<Cpu> = (0..17).map(|i| cpu_from(i, None)).collect();
    assert_eq!(package_counts(&odd, &odd[16]), (17, 17));
    let m = Common {
        vendor: String::from("GenuineIntel"),
        family: 6,
        model: 0,
        brand: String::new(),
        stepping: 0,
        tsc_hz: 0,
        cache_kb: 0,
        level: 0,
        flags: String::new(),
        tlb: 0,
        clflush: 0,
        phys: 0,
        virt: 0,
        power: 0,
    };
    let mut s = String::new();
    block(&mut s, &m, &one[0], 1, 1);
    assert!(s.starts_with("processor\t: 0\n") && s.contains("siblings\t: 1\n") && s.contains("cpu cores\t: 1\n") && s.ends_with("\n\n"), "{s}");
}

#[test]
fn smt_files() {
    use super::cpuinfo::cpu_from;
    use super::sys_cpu::{smt_active, smt_control_store, smt_control_text};
    let flat: alloc::vec::Vec<Cpu> = (0..3).map(|i| cpu_from(i, None)).collect();
    assert!(!smt_active(&flat));
    let smt: alloc::vec::Vec<Cpu> = (0..4).map(|i| Cpu { cpu: i, pkg: 0, die: 0, cluster: 0, core: i as u32 / 2, apic: i as u32 }).collect();
    assert!(smt_active(&smt));
    assert!(!smt_active(&smt[..1]));
    assert_eq!(smt_control_text(2), "on\n");
    assert_eq!(smt_control_text(1), "notsupported\n");
    assert_eq!(smt_control_store(1, b"on"), Err(ENODEV));
    assert_eq!(smt_control_store(2, b"on\n"), Ok(()));
    assert_eq!(smt_control_store(2, b"2"), Ok(()));
    assert_eq!(smt_control_store(2, b"off\n"), Err(EBUSY));
    assert_eq!(smt_control_store(2, b"1"), Err(EBUSY));
    assert_eq!(smt_control_store(2, b"3"), Err(EINVAL));
    assert_eq!(smt_control_store(2, b"maybe"), Err(EINVAL));
}

#[test]
fn cache_leaves_as_cacheinfo_shows_them() {
    use super::sys_cache::{cache_id, count_order, decode, sharers};
    use hal::cpu::CpuidResult;
    // Zen 4 L1d: type 1, level 1, 2 threads share it, 64-byte lines, 8 ways, 64 sets.
    let l1 = decode(CpuidResult { eax: 1 | (1 << 5) | (1 << 14), ebx: 63 | (7 << 22), ecx: 63, edx: 0 }).unwrap();
    assert_eq!((l1.level, l1.type_name(), l1.line, l1.ways, l1.sets, l1.partitions, l1.sharing), (1, "Data", 64, 8, 64, 1, 2));
    assert_eq!(l1.size() >> 10, 32);
    assert_eq!(decode(CpuidResult { eax: 0, ebx: 0, ecx: 0, edx: 0 }), None);
    assert_eq!((count_order(1), count_order(2), count_order(3), count_order(16)), (0, 1, 2, 4));
    assert_eq!(cache_id(5, &l1), 2);
    let all: alloc::vec::Vec<Cpu> = (0..16).map(|i| Cpu { cpu: i, pkg: 0, die: 0, cluster: (i / 8) as u32, core: i as u32 / 2, apic: i as u32 }).collect();
    assert_eq!(sharers(&all, &all[5], 0, &l1, true), alloc::vec![4, 5]);
    assert_eq!(sharers(&all, &all[5], 0, &l1, false), alloc::vec![4, 5]);
    // L3 (index 3 on AMD): the LLC domain.
    let l3 = decode(CpuidResult { eax: 3 | (3 << 5) | (15 << 14), ebx: 63 | (15 << 22), ecx: 16383, edx: 0 }).unwrap();
    assert_eq!((l3.type_name(), l3.size() >> 10), ("Unified", 16384));
    assert_eq!(sharers(&all, &all[9], 3, &l3, true), (8..16).collect::<alloc::vec::Vec<_>>());
    assert_eq!(sharers(&all[..1], &all[0], 0, &l1, true), alloc::vec![0]);
}

#[test]
fn sysctl_and_net_writes_as_linux() {
    use super::sys_net::{duplex_text, kstrtoul, speed_text, store_result, uevent_store};
    use super::sysctl::ratio_of;
    assert_eq!(ratio_of(0, 0), 50);
    assert_eq!(ratio_of(3_800, 4_000), 95);
    assert_eq!(ratio_of(9, 4), 100);
    assert_eq!(kstrtoul(b"1500\n"), Ok(1500));
    assert_eq!(kstrtoul(b"0x10"), Ok(16));
    assert_eq!(kstrtoul(b"010"), Ok(8));
    assert_eq!(kstrtoul(b"0"), Ok(0));
    assert_eq!(kstrtoul(b""), Err(EINVAL));
    assert_eq!(kstrtoul(b"12 "), Err(EINVAL));
    assert_eq!(kstrtoul(b"-1"), Err(EINVAL));
    assert_eq!(kstrtoul(b"99999999999999999999"), Err(ERANGE));
    let card = net::ifview::Link {
        index: 2,
        name: String::from("igc0"),
        loopback: false,
        mac: [0; 6],
        mtu: 1500,
        carrier: true,
        carrier_up: 1,
        carrier_down: 0,
        stats: Default::default(),
        addrs: vec![],
    };
    assert_eq!(store_result("mtu", &card, 1500), Ok(()));
    assert_eq!(store_result("mtu", &card, 9000), Err(EOPNOTSUPP));
    assert_eq!(store_result("mtu", &card, 67), Err(EINVAL));
    assert_eq!(store_result("mtu", &card, 9217), Err(EINVAL));
    assert_eq!(store_result("mtu", &card, u32::MAX as u64), Err(EINVAL));
    assert_eq!(store_result("tx_queue_len", &card, 1000), Ok(()));
    assert_eq!(store_result("tx_queue_len", &card, 1 << 32), Err(ERANGE));
    assert_eq!(store_result("carrier", &card, 1), Err(EOPNOTSUPP));
    assert_eq!(store_result("netdev_group", &card, 0), Ok(()));
    let lo = net::ifview::Link { name: String::from("lo"), loopback: true, mtu: 65536, ..card };
    assert_eq!(store_result("mtu", &lo, 0), Err(EOPNOTSUPP));
    assert_eq!(store_result("mtu", &lo, 65536), Ok(()));
    assert_eq!(store_result("mtu", &lo, 1 << 31), Err(EINVAL));
    assert_eq!((speed_text(Some(2500)), duplex_text(Some(2500))), (String::from("2500\n"), String::from("full\n")));
    assert_eq!((speed_text(None), duplex_text(None)), (String::from("-1\n"), String::from("unknown\n")));
    assert_eq!(duplex_text(Some(100)), "unknown\n");
    let root = vfs::Cred { uid: 0, gid: 0 };
    assert_eq!(uevent_store(root, b"change\n"), Ok(7));
    assert_eq!(uevent_store(root, b"explode"), Err(EINVAL));
}
