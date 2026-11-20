//! SYS-A: "Bericht für Claude kopieren" (a plain-text summary of the machine and BitsOS for the clipboard) and the
//! text of a tab for `ctl system` (scenarios and the Mini-PC checks read it). The report carries no secret and no
//! serial number: the sampler reads no serial, UUID, MAC or IP address to begin with.

use crate::sys_model::{Desk, Item, TabModel};
use crate::sys_snap::Snapshot;
use crate::sys_text as tx;

pub fn text(s: &Snapshot, d: &Desk, off: i32) -> String {
    let mut o = String::from("BitsOS-Systembericht (aus dem Programm „System“; ohne Seriennummern und Netzadressen)\n");
    let mut line = |k: &str, v: String| o.push_str(&format!("{k}: {v}\n"));
    let k = &s.kernel;
    line("BitsOS", k.generation.as_ref().map_or_else(|| "V2".into(), |(g, v)| format!("V2, Generation {g}, Kern {v}")));
    line("Gestartet", format!("{}, Hochfahren {}, läuft seit {}", k.boot_wall.map_or_else(|| tx::NONE.into(), |w| tx::date_time(w, off)), k.boot_ms.map_or_else(|| tx::NONE.into(), |ms| format!("{} s", tx::dec(ms as f64 / 1000.0, 1))), tx::duration(k.uptime)));
    line("Letzter Absturz", k.crash.map_or_else(|| "keiner gespeichert".into(), |(w, g)| format!("{} (Generation {g})", tx::date_time(w, off))));
    if let Some(n) = k.crashes {
        line("Abstürze seit dem Start", tx::int(n as u64));
    }
    let dmi: Vec<String> = s.facts.dmi.iter().map(|(k, v)| format!("{k} {v}")).collect();
    if !dmi.is_empty() {
        line("Rechner", dmi.join(", "));
    }
    let f = &s.facts;
    line(
        "Prozessor",
        format!(
            "{}, {} Kerne, {} Threads, Takt {} (höchstens {}), Auslastung {}, Temperatur {}, Paket {}",
            f.cpu_model.clone().unwrap_or_else(|| tx::NONE.into()),
            f.cores.map_or_else(|| tx::NONE.into(), |n| n.to_string()),
            f.threads.map_or_else(|| tx::NONE.into(), |n| n.to_string()),
            tx::khz(s.cpu.cur_khz),
            tx::khz(s.cpu.max_khz),
            tx::pct(s.cpu.busy),
            tx::celsius(s.heat.cpu),
            tx::watt(s.heat.cpu_power)
        ),
    );
    if let Some(m) = s.mem.mem {
        line("Arbeitsspeicher", format!("{} belegt von {}, {} frei verfügbar, Auslagerung {}", tx::bytes(Some(m.used())), tx::bytes(Some(m.total)), tx::bytes(Some(m.available)), tx::bytes(Some(m.swap_total))));
    }
    let g = &s.gpu;
    line(
        "Grafik",
        format!(
            "{}, Auslastung {}, fest {}, aus dem Arbeitsspeicher {}, Temperatur {}, Strom {}",
            f.gpu_name.clone().unwrap_or_else(|| tx::NONE.into()),
            tx::pct(g.busy),
            g.vram.map_or_else(|| tx::NONE.into(), |v| format!("{} / {}", tx::bytes(Some(v.0)), tx::bytes(Some(v.1)))),
            g.gtt.map_or_else(|| tx::NONE.into(), |v| format!("{} / {}", tx::bytes(Some(v.0)), tx::bytes(Some(v.1)))),
            tx::celsius(g.temp.or(s.heat.gpu)),
            tx::watt(g.power.or(s.heat.gpu_power))
        ),
    );
    line(
        "Bildschirm",
        format!(
            "{}, {}, gezeichnet mit der Grafik",
            d.size.map_or_else(|| tx::NONE.into(), |(w, h)| format!("{w} × {h}")),
            d.refresh_hz.map_or_else(|| tx::NONE.into(), |h| format!("{} Hz", tx::dec(h, 0))),
        ),
    );
    for disk in &s.disks {
        let parts: Vec<String> = disk.parts.iter().map(|(n, sz)| format!("{n} {}", tx::bytes(Some(*sz)))).collect();
        line("Datenträger", format!("{} {}{}, Teile: {}", disk.name, disk.model.clone().map(|m| format!("({m}) ")).unwrap_or_default(), tx::bytes(disk.size), if parts.is_empty() { "keine".into() } else { parts.join(", ") }));
    }
    for m in &s.mounts {
        line("Eingehängt", format!("{} {} / {}", m.path, tx::bytes(Some(m.used)), tx::bytes(Some(m.total))));
    }
    for n in &s.nets {
        let state = match n.up {
            Some(true) => "verbunden",
            Some(false) => "nicht verbunden",
            None => "–",
        };
        line("Netz", format!("{} {state}{}", n.name, n.speed_mbps.map(|sp| format!(", {sp} Mbit/s")).unwrap_or_default()));
    }
    let usb: Vec<String> = f.usb.iter().map(|(n, sp)| format!("{n} ({sp})")).collect();
    if !usb.is_empty() {
        line("USB", usb.join(", "));
    }
    let missing = missing(s);
    if !missing.is_empty() {
        line("Noch ohne Messquelle", missing.join(", "));
    }
    o
}

/// The measurements the kernel does not provide yet (K1 of SYS-A), in words.
pub fn missing(s: &Snapshot) -> Vec<&'static str> {
    let mut v = Vec::new();
    let mut need = |ok: bool, what: &'static str| {
        if !ok {
            v.push(what);
        }
    };
    need(s.cpu.cur_khz.is_some(), "Prozessortakt");
    need(s.heat.cpu.is_some(), "Prozessortemperatur");
    need(s.heat.cpu_power.is_some(), "Strom des Prozessors");
    need(s.gpu.busy.is_some(), "Auslastung der Grafik");
    need(s.gpu.vram.is_some(), "Grafikspeicher");
    need(!s.metrics.is_empty(), "Kernzähler (/proc/bitsos/metrics)");
    need(!s.facts.dmi.is_empty(), "Hersteller und Modell");
    v
}

/// A tab as text lines (`ctl system`): each card's items as `label | value`, the task manager's lines.
pub fn tab_text(m: &TabModel) -> String {
    let mut o = format!("sub | {}\n", m.sub);
    if let Some((s, _)) = &m.status {
        o.push_str(&format!("status | {s}\n"));
    }
    for (ri, row) in m.rows.iter().enumerate() {
        for (ci, card) in row.cards.iter().enumerate() {
            o.push_str(&format!("[card {ri}.{ci}]\n"));
            items(&card.items, &mut o);
        }
    }
    if let Some(p) = &m.procs {
        o.push_str(&format!("counts | {} {} {} {}\n", p.counts[0], p.counts[1], p.counts[2], p.counts[3]));
        for s in &p.sections {
            o.push_str(&format!("[{}]\n", s.head));
            for l in &s.lines {
                o.push_str(&format!("{} | {} | {} | {} | {}\n", l.pid, l.name, l.note, l.state, l.cells.join(" | ")));
            }
            if s.more > 0 {
                o.push_str(&format!("… {} weitere\n", s.more));
            }
        }
        if let Some(d) = &p.detail {
            o.push_str(&format!("[detail {}]\nname | {}\nline | {}\n", d.pid, d.name, d.line));
            for (k, v) in &d.rows {
                o.push_str(&format!("{k} | {v}\n"));
            }
            o.push_str(&format!("vorrang | {}\nend | {:?}\nprio | {:?}\nnote | {}\n", d.vorrang.word(), d.end, d.prio, d.note));
        }
    }
    if let Some(d) = &m.devices {
        for (g, devs) in &d.groups {
            o.push_str(&format!("[{g}]\n"));
            for (n, s, _) in devs {
                o.push_str(&format!("{n} | {s}\n"));
            }
        }
        o.push_str("[detail]\n");
        items(&d.detail, &mut o);
        o.push_str("[facts]\n");
        items(&d.facts, &mut o);
    }
    o
}

fn items(v: &[Item], o: &mut String) {
    for it in v {
        let l = match it {
            Item::Label(s) => format!("# {s}"),
            Item::LabelRight(s, _) => format!("# {s}"),
            Item::Big(a, b) => format!("big | {a} | {b}"),
            Item::Mid(a) => format!("mid | {a}"),
            Item::Sub(a) => format!("sub | {a}"),
            Item::Row(a, b) | Item::RowTone(a, b, _) => format!("{a} | {b}"),
            Item::Grid(g) => g.iter().map(|(a, b)| format!("{a} | {b}")).collect::<Vec<_>>().join("\n"),
            Item::Link(a, p) => format!("link | {a} | {}", crate::sys_view::part_name(*p)),
            Item::Buttons(b) => b.iter().map(|(a, p, _)| format!("button | {a} | {}", crate::sys_view::part_name(*p))).collect::<Vec<_>>().join("\n"),
            Item::Chart(c) => format!("chart | {} series | {} points", c.series.len(), c.series.first().map_or(0, |s| s.0.iter().filter(|v| v.is_some()).count())),
            Item::Threads(t) => format!("threads | {}", t.len()),
            Item::Bar(_) | Item::Legend(_) | Item::Rule | Item::Fill => continue,
        };
        o.push_str(&l);
        o.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys_snap::{Disk, Facts, Kernel, Net};

    #[test]
    fn the_report_names_the_facts_and_no_secrets() {
        let s = Snapshot {
            facts: Facts { cpu_model: Some("AMD Ryzen 7 PRO 8845HS".into()), cores: Some(8), threads: Some(16), dmi: vec![("Hersteller".into(), "Acme".into())], usb: vec![("Tastatur".into(), "USB 2".into())], ..Facts::default() },
            kernel: Kernel { generation: Some((151, "0.1.0".into())), uptime: Some(6840.0), ..Kernel::default() },
            disks: vec![Disk { name: "nvme0n1".into(), size: Some(1 << 40), parts: vec![("nvme0n1p1".into(), 1 << 30)], ..Disk::default() }],
            nets: vec![Net { name: "igc0".into(), up: Some(true), speed_mbps: Some(2500), ..Net::default() }],
            ..Snapshot::default()
        };
        let t = text(&s, &Desk { size: Some((2560, 1440)), refresh_hz: Some(60.0), ..Desk::default() }, 60);
        for want in ["V2, Generation 151, Kern 0.1.0", "AMD Ryzen 7 PRO 8845HS, 8 Kerne, 16 Threads", "läuft seit 1 Std 54 Min", "nvme0n1 1,0 TB", "igc0 verbunden, 2500 Mbit/s", "Tastatur (USB 2)", "2560 × 1440, 60 Hz, gezeichnet mit der Grafik", "Hersteller Acme", "Noch ohne Messquelle: Prozessortakt"] {
            assert!(t.contains(want), "{want} missing in\n{t}");
        }
        for never in ["serial", "Serien-Nr", "uuid", "token", "psk"] {
            assert!(!t.to_lowercase().contains(never), "{never} in\n{t}");
        }
    }
}
