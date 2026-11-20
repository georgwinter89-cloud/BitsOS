//! SYS-A: the sampler on a fixture tree: CPU shares from two samples (whole processor, per thread, per process),
//! rates, missing sources as None, the facts.

use super::*;

fn put(root: &std::path::Path, rel: &str, text: &str) {
    let p = root.join(rel.trim_start_matches('/'));
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn tree(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("bitscomp-sys-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn step(root: &std::path::Path, k: u64) {
    // 1000 ticks a step over two threads: 60 busy (40 programs, 20 kernel); thread 0 busy 50 of its 500, thread 1 10.
    put(root, "/proc/stat", &format!("cpu  {} 0 {} {} 0 0 0 0\ncpu0 {} 0 {} {} 0 0 0 0\ncpu1 {} 0 0 {} 0 0 0 0\nctxt {}\n", 40 * k, 20 * k, 940 * k, 30 * k, 20 * k, 450 * k, 10 * k, 490 * k, 8400 * k));
    put(root, "/proc/77/stat", &format!("77 (game x) R 1 77 77 0 -1 0 0 0 0 0 {} {} 0 0 20 0 3 0 500 1000 250\n", 30 * k, 11 * k));
    put(root, "/proc/77/io", &format!("rchar: 0\nwchar: 0\nread_bytes: {}\nwrite_bytes: 0\n", 1_048_576 * k));
    put(root, "/proc/net/dev", &format!("h\nh\n  igc0: {} 1 0 0 0 0 0 0 {} 1 0 0 0 0 0 0\n", 2_000_000 * k, 100_000 * k));
}

#[test]
fn shares_and_rates_come_from_two_samples() {
    let root = tree("two");
    put(&root, "/proc/cpuinfo", "processor\t: 0\nmodel name\t: Test CPU\ncpu cores\t: 1\n\nprocessor\t: 1\nmodel name\t: Test CPU\n");
    put(&root, "/proc/77/status", "Name:\tgame\nUid:\t1000\t1000\t1000\t1000\nVmRSS:\t1024 kB\n");
    put(&root, "/proc/77/cmdline", "game\0--x\0");
    step(&root, 1);
    let mut s = Sampler::new(Root(root.clone()));
    let want = Want { procs: true, gpu_procs: false, day: false };
    let first = s.sample(want, 10.0, 0);
    assert_eq!(first.cpu.busy, None, "one look has no share yet");
    assert_eq!(first.procs[0].cpu, None);
    assert_eq!(first.facts.threads, Some(2));
    step(&root, 3);
    let two = s.sample(want, 12.0, 0);
    assert_eq!(two.cpu.busy, Some(6.0));
    assert_eq!(two.cpu.programs, Some(4.0));
    assert_eq!(two.cpu.kernel, Some(2.0));
    assert_eq!(two.cpu.threads, vec![Some(10.0), Some(2.0)]);
    assert_eq!(two.cpu.switches, Some(8400.0), "context switches per second over the 2 s");
    let p = &two.procs[0];
    assert_eq!((p.pid, p.comm.as_str(), p.uid, p.rss, p.argv.len()), (77, "game x", Some(1000), Some(1024 * 1024), 2));
    assert_eq!(p.cpu, Some(4.1), "(30 + 11) ticks of 1000 a step: 4,1 % of the whole processor");
    assert_eq!(p.disk, Some(1_048_576.0), "1 MiB a step, two steps in two seconds");
    assert_eq!(two.nets[0].rx, Some(2_000_000.0));
    assert_eq!(two.nets[0].tx, Some(100_000.0));
    assert_eq!(two.nets[0].rx_total, 6_000_000);
    // Missing sources are None, not 0.
    assert_eq!((two.heat.cpu, two.gpu.busy, two.cpu.cur_khz, two.mem.mem), (None, None, None, None));
    assert!(two.kernel.generation.is_none() && two.mounts.is_empty());
    // A reused pid (another start time) has no share from the old one's ticks.
    put(&root, "/proc/77/stat", "77 (other) S 1 77 77 0 -1 0 0 0 0 0 999 0 0 0 20 0 1 0 900 1000 250\n");
    step_cpu_only(&root, 4);
    let three = s.sample(want, 13.0, 0);
    assert_eq!(three.procs[0].cpu, None, "not the same process");
    // Without the process list the sampler forgets the processes' ticks (nothing for hidden tabs).
    let four = s.sample(Want::default(), 14.0, 0);
    assert!(four.procs.is_empty() && !four.procs_read);
    let _ = std::fs::remove_dir_all(&root);
}

fn step_cpu_only(root: &std::path::Path, k: u64) {
    put(root, "/proc/stat", &format!("cpu  {} 0 {} {} 0 0 0 0\nctxt 1\n", 40 * k, 20 * k, 940 * k));
}

#[test]
fn k1_files_appear_without_a_restart() {
    let root = tree("k1");
    step(&root, 1);
    let mut s = Sampler::new(Root(root.clone()));
    let a = s.sample(Want::default(), 1.0, 0);
    assert_eq!(a.heat.cpu, None);
    put(&root, "/sys/class/hwmon/hwmon0/name", "k10temp\n");
    put(&root, "/sys/class/hwmon/hwmon0/temp1_input", "52125\n");
    put(&root, "/sys/class/hwmon/hwmon0/temp1_label", "Tctl\n");
    // A battery's sensor comes first and is not the processor (only CPU_POWER's chips are).
    put(&root, "/sys/class/hwmon/hwmon0b/name", "BAT0\n");
    put(&root, "/sys/class/hwmon/hwmon0b/power1_input", "7000000\n");
    put(&root, "/sys/class/hwmon/hwmon1/name", "cpu_package\n");
    put(&root, "/sys/class/hwmon/hwmon1/power1_average", "18000000\n");
    put(&root, "/sys/class/hwmon/hwmon2/name", "amdgpu\n");
    put(&root, "/sys/class/hwmon/hwmon2/temp1_input", "48000\n");
    put(&root, "/sys/class/hwmon/hwmon2/power1_average", "9000000\n");
    put(&root, "/sys/class/drm/card0/device/gpu_busy_percent", "31\n");
    put(&root, "/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq", "3800000\n");
    put(&root, "/proc/bitsos/metrics", "crash.since_boot 1 n\nmm.oom.kills 0 n\n");
    put(&root, "/statfs", "/data 100 1000\n");
    put(&root, "/proc/mounts", "/dev/nvme0n1p3 /data ext4 rw 0 0\nproc /proc proc rw 0 0\n");
    let b = s.sample(Want::default(), 2.0, 0);
    assert_eq!(b.heat.cpu, Some(52.125));
    assert_eq!((b.heat.cpu_power, b.heat.gpu_power, b.heat.gpu), (Some(18.0), Some(9.0), Some(48.0)));
    // Without the package chip: not the battery, but the kernel's series (lane SO names it power.pkg).
    let _ = std::fs::remove_dir_all(root.join("sys/class/hwmon/hwmon1"));
    let c = s.sample(Want::default(), 3.0, 0);
    assert_eq!(c.heat.cpu_power, None, "a battery is not the processor");
    put(&root, "/proc/bitsos/metrics", "power.pkg 9.8 W\ncrash.kernel.previous_start 1 n\ncrash.programs 0 n\n");
    let d = s.sample(Want::default(), 4.0, 0);
    assert_eq!(d.heat.cpu_power, Some(9.8));
    assert_eq!(d.kernel.crashes, Some(0.0), "a crash of the start before is not one since this start");
    assert_eq!((b.gpu.busy, b.gpu.via), (Some(31.0), "sysfs"));
    assert_eq!(b.cpu.cur_khz, Some(3_800_000));
    assert_eq!((b.kernel.crashes, b.kernel.oom), (Some(1.0), Some(0.0)));
    assert_eq!(b.mounts, vec![crate::sys_snap::Mount { dev: "/dev/nvme0n1p3".into(), path: "/data".into(), used: 100, total: 1000 }]);
    let _ = std::fs::remove_dir_all(&root);
}
