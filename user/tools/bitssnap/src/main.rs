//! The `bitssnap` command.
#![forbid(unsafe_code)]

use bitssnap::dev::Dev;
use bitssnap::{human, local_time, restore, serve, settings, space};
use std::process::exit;

const USAGE: &str = "usage: bitssnap COMMAND
  list [--sizes]            snapshots of /data, oldest first (local time, kind, own size)
  take [--label NAME]       a snapshot now (kept until deleted by hand)
  restore ID PATH           a file or folder below /data back as in snapshot ID (the state before is saved
                            first as a snapshot \"before-restore\": restoring from that one undoes it)
  restore-program ID NAME [--root DIR]
                            program NAME as in snapshot ID: its package version (/apps/NAME/current then)
                            and its data folder /data/appdata/NAME (one \"before-restore\" snapshot first)
  rollback ID | --cancel    the whole /data back to snapshot ID at the next start (the state then is kept
                            as \"before-rollback\": a rollback to it undoes this); no argument: what is set
  restore-system TIME [--dry-run] | --undo
                            everything as at TIME (YYYY-MM-DD HH:MM local, or UTC seconds): /data's snapshot
                            of then and the system generation of then (system_pin), at the next start;
                            --undo takes back this start's last one (only what it set)
  delete ID                 delete a snapshot
  mount ID | umount ID      snapshot ID read-only at /run/bitsos/snap/ID
  status                    how full the pool is, the snapshots by kind, freeze pauses; above 80 % full a
                            warning and the oldest manual snapshots proposed for deletion (none deleted)
  stats                     the kernel's raw pool numbers
  prune [--dry-run]         apply the retention rules now (snap.* settings)
  serve [--once]            the scheduler init starts (every snap.interval_min minutes), MCP tools
                            snapshots, snapshot_take, snapshot_delete and restore";

fn fail(m: impl std::fmt::Display) -> ! {
    eprintln!("bitssnap: {m}");
    exit(1)
}

fn usage() -> ! {
    eprintln!("{USAGE}");
    exit(2)
}

fn dev() -> Dev {
    Dev::open().unwrap_or_else(|e| fail(e))
}

fn id(a: Option<&String>) -> u64 {
    a.and_then(|s| s.parse().ok()).unwrap_or_else(|| usage())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["--help" | "-h" | "help"] => println!("{USAGE}"),
        ["list"] | ["list", "--sizes"] => {
            let mut d = dev();
            let st = d.stats().unwrap_or_else(|e| fail(e));
            let l = d.list(a.len() == 2).unwrap_or_else(|e| fail(e));
            println!("{:>6}  {:<16}  {:<14}  {:>9}  label", "id", "time", "kind", "size");
            for s in &l {
                let kind = s.kind();
                let size = s.bytes.map(human).unwrap_or_else(|| "-".into());
                let m = if s.mounted { "  (mounted)" } else { "" };
                println!("{:>6}  {:<16}  {kind:<14}  {size:>9}  {}{m}", s.id, local_time(s.created_ns / 1_000_000_000, st.utc_offset_min * 60), s.label);
            }
            println!("{} snapshots, {}.{} % of the pool free", l.len(), st.free_permille / 10, st.free_permille % 10);
        }
        ["take"] | ["take", "--label", _] => {
            let label = a.get(2).copied().unwrap_or("manual");
            let t = dev().take(label, false, false).unwrap_or_else(|e| fail(e));
            println!("snapshot {} taken ({} ms pause)", t.id, t.pause_us / 1000);
        }
        ["restore", _, path] => {
            let i = id(args.get(1));
            let r = restore::restore(&mut dev(), i, path).unwrap_or_else(|e| fail(e));
            println!(
                "{} restored from snapshot {i}: {} files, {} folders, {} links, {} hard links, {}",
                r.path,
                r.files,
                r.dirs,
                r.links,
                r.hardlinks,
                human(r.bytes)
            );
            for s in &r.special {
                println!("not restored (socket, FIFO or device node): {s}");
            }
            println!("the state before is snapshot {} (undo: bitssnap restore {} {path})", r.before, r.before);
        }
        ["restore-program", _, name] | ["restore-program", _, name, "--root", _] => {
            let root = std::path::PathBuf::from(a.get(4).copied().unwrap_or("/"));
            let r = restore::program::restore_program(&mut dev(), id(args.get(1)), name, &root).unwrap_or_else(|e| fail(e));
            println!("{}", r.text());
        }
        ["rollback"] | ["rollback", _] => {
            use restore::volume;
            let mut d = dev();
            let r = match a.get(1).copied() {
                None => volume::status(&mut d),
                Some("--cancel") => volume::cancel(&mut d),
                Some(_) => volume::schedule(&mut d, id(args.get(1))),
            };
            println!("{}", volume::describe(&r.unwrap_or_else(|e| fail(e))));
        }
        ["restore-system", "--undo"] => println!("{}", restore::volume::undo_system(&mut dev()).unwrap_or_else(|e| fail(e)).1),
        ["restore-system", at] | ["restore-system", at, "--dry-run"] => {
            let mut d = dev();
            let off = d.stats().unwrap_or_else(|e| fail(e)).utc_offset_min;
            let utc = restore::volume::parse_at(at, off).unwrap_or_else(|e| fail(e));
            let dry = a.len() == 3;
            let r = restore::volume::restore_system(&mut d, utc, dry).unwrap_or_else(|e| fail(e));
            println!("{}", restore::volume::system_text(&r, dry, off * 60));
        }
        ["delete", _] => {
            let i = id(args.get(1));
            dev().delete(i).unwrap_or_else(|e| fail(e));
            println!("snapshot {i} deleted");
        }
        ["mount", _] => println!("{}", dev().mount(id(args.get(1))).unwrap_or_else(|e| fail(e))),
        ["umount", _] => dev().umount(id(args.get(1))).unwrap_or_else(|e| fail(e)),
        ["status"] => status(),
        ["stats"] => println!("{}", dev().stats().unwrap_or_else(|e| fail(e)).raw),
        ["prune"] | ["prune", "--dry-run"] => {
            let (s, why) = settings::load(settings::SOCKET);
            if let Some(w) = why {
                eprintln!("bitssnap: settings: defaults ({w})");
            }
            let dry = a.len() == 2;
            let r = serve::round(&mut dev(), &s, false, dry).unwrap_or_else(|e| fail(e));
            let verb = if dry { "would delete" } else { "deleted" };
            println!("{verb} by age: {:?}; for space: {:?}; {}.{} % free", r.deleted_age, r.deleted_space, r.free_permille / 10, r.free_permille % 10);
        }
        ["serve"] => exit(serve::serve(false)),
        ["serve", "--once"] => exit(serve::serve(true)),
        _ => usage(),
    }
}

/// `bitssnap status`: the pool, the snapshots by kind, the pauses and the space warning (VS-6).
fn status() {
    let mut d = dev();
    let (st, sp) = space::read(&mut d).unwrap_or_else(|e| fail(e));
    let l = d.list(false).unwrap_or_else(|e| fail(e));
    for line in space::status_lines(&st, &l, &sp) {
        println!("{line}");
    }
}
