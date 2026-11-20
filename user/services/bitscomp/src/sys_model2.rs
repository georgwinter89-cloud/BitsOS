//! SYS-A: the tabs Grafik, Datenträger, Netz, Strom und Wärme and Verlauf of the program "System" (see sys_model).

use crate::sys_model::{Chart, Ctx, Item, MASK, Shape, TabModel, Tone, card, disk_rates, heat_words, mount_name, net_now, row};
use crate::sys_ring::Range;
use crate::sys_text as tx;

fn pair(c: &Ctx, v: Option<(u64, u64)>) -> (String, Vec<(f64, Tone)>) {
    match v {
        Some((u, t)) if t > 0 => (c.m(format!("{} / {}", tx::bytes(Some(u)), tx::bytes(Some(t)))), if c.v.mask { Vec::new() } else { vec![((u as f64 / t as f64).min(1.0), Tone::Blue)] }),
        _ => (tx::NONE.into(), Vec::new()),
    }
}

pub fn gpu(c: &Ctx) -> TabModel {
    let s = c.s;
    let g = &s.gpu;
    let clock = g.sclk_mhz.map_or_else(|| tx::NO_SOURCE.to_string(), |m| format!("Takt {}", tx::khz(Some((m * 1000.0) as u64))));
    let busy = card(1, vec![Item::Label("Auslastung der Grafik".into()), Item::Big(c.m(tx::pct0(g.busy)), c.m(clock)), c.chart(vec![("gpu", Tone::Blue, false)], Some(100.0), 160)]);
    let hz = c.desk.refresh_hz;
    let frame = hz.map(|h| 1000.0 / h);
    let mut fps = Item::Chart(Chart { series: vec![(c.pts("desk.frame_ms"), Tone::Accent, false)], max: frame.map(|f| f * 2.0), shape: Shape::Bars, axis: Some(c.v.range.axis()), h: 160, mark: frame.map(|f| (f, tx::ms(Some(f)))) });
    if c.v.mask {
        fps = Item::Chart(Chart { series: Vec::new(), max: Some(1.0), shape: Shape::Bars, axis: Some(c.v.range.axis()), h: 160, mark: None });
    }
    let fps_card = card(1, vec![
        Item::LabelRight("Bilder pro Sekunde · Desktop".into(), vec![(Tone::Muted, hz.map_or_else(String::new, |h| format!("Bildschirm {} Hz", tx::dec(h, 0))))]),
        Item::Big(c.m(c.desk.fps.map_or_else(|| tx::NONE.into(), |f| tx::dec(f, 0))), "Zeit pro Bild, je Sekunde gemittelt".into()),
        fps,
        Item::Sub("Gestrichelt: so lang darf ein Bild höchstens brauchen.".into()),
    ]);
    let (vram, vbar) = pair(c, g.vram);
    let (gtt, gbar) = pair(c, g.gtt);
    let mem = card(1, vec![
        Item::Label("Grafikspeicher".into()),
        Item::Row("Fest zugeteilt".into(), vram),
        Item::Bar(vbar),
        Item::Row("Aus dem Arbeitsspeicher".into(), gtt),
        Item::Bar(gbar),
        Item::Sub(if g.vram.is_none() { tx::NO_SOURCE.into() } else { "Die Grafik teilt sich den Arbeitsspeicher mit dem Prozessor.".into() }),
    ]);
    let heat = card(1, vec![
        Item::Label("Wärme und Strom".into()),
        Item::Grid(vec![
            ("Temperatur".into(), c.m(tx::celsius(g.temp.or(s.heat.gpu)))),
            ("Strom".into(), c.m(tx::watt(g.power.or(s.heat.gpu_power)))),
            ("Speichertakt".into(), c.m(g.mclk_mhz.map_or_else(|| tx::NONE.into(), |m| tx::khz(Some((m * 1000.0) as u64))))),
            ("Spannung".into(), c.m(g.volt.map_or_else(|| tx::NONE.into(), |v| format!("{} V", tx::dec(v, 2))))),
        ]),
    ]);
    let d = c.desk;
    let desk = card(1, vec![
        Item::Label("Der Desktop".into()),
        Item::Row("Bild bauen".into(), c.m(tx::ms(d.build_ms))),
        Item::Row("Bild zeichnen".into(), c.m(tx::ms(d.render_ms))),
        Item::Row("Bis es zu sehen ist".into(), c.m(tx::ms(d.present_ms))),
        Item::Row("Eingabe bis Bild".into(), c.m(tx::ms(d.input_ms))),
        Item::Row("Zeichnet mit".into(), "der Grafik".into()),
    ]);
    let errors = s.metrics.iter().filter(|(n, _, _)| n.starts_with("drm.") && (n.contains("error") || n.contains("hang") || n.contains("reset"))).map(|(_, v, _)| *v).fold(None, |a: Option<f64>, x| Some(a.unwrap_or(0.0) + x));
    let screen = card(1, vec![
        Item::Label("Bildschirm".into()),
        Item::Row("Auflösung".into(), d.size.map_or_else(|| tx::NONE.into(), |(w, h)| format!("{w} × {h}"))),
        Item::Row("Bildwechsel".into(), hz.map_or_else(|| tx::NONE.into(), |h| format!("{} Hz", tx::dec(h, 0)))),
        Item::Row("Bilder seit dem Start".into(), c.m(tx::int(d.frames))),
        Item::Row("Fehler der Grafik".into(), errors.map_or_else(|| tx::NONE.into(), |n| if n == 0.0 { "keine".into() } else { c.m(tx::int(n as u64)) })),
    ]);
    let sub = match (&s.facts.gpu_name, g.via) {
        (Some(n), "amdgpu") => format!("{n} · Werte vom Treiber"),
        (Some(n), _) => n.clone(),
        (None, _) => "Keine Grafikkarte gefunden".into(),
    };
    TabModel { sub, rows: vec![row(vec![busy, fps_card], false), row(vec![mem, heat, desk, screen], true)], ..TabModel::default() }
}

pub fn disks(c: &Ctx) -> TabModel {
    let s = c.s;
    let mut list = vec![Item::Label("Eingehängt".into())];
    for m in &s.mounts {
        list.push(Item::Row(mount_name(&m.path), c.m(format!("{} / {}", tx::bytes(Some(m.used)), tx::bytes(Some(m.total))))));
        list.push(Item::Bar(if c.v.mask { Vec::new() } else { vec![((m.used as f64 / m.total.max(1) as f64).min(1.0), Tone::Accent)] }));
    }
    if s.mounts.is_empty() {
        list.push(Item::Sub("Keine Datenträger eingehängt.".into()));
    }
    let (r, w) = disk_rates(c);
    let chart = card(2, vec![
        Item::LabelRight("Lesen und Schreiben".into(), vec![(Tone::Accent, "Lesen".into()), (Tone::Amber, "Schreiben".into())]),
        Item::Big(c.m(tx::rate(r)), format!("Schreiben {}", c.m(tx::rate(w)))),
        c.chart(vec![("disk.read", Tone::Accent, true), ("disk.write", Tone::Amber, false)], None, 200),
    ]);
    let mut devs = Vec::new();
    for d in &s.disks {
        let mut items = vec![Item::Label(d.model.clone().map_or_else(|| d.name.clone(), |m| format!("{} · {m}", d.name)))];
        items.push(Item::Row("Größe".into(), tx::bytes(d.size)));
        items.push(Item::Row("Art".into(), match d.rotational {
            Some(true) => "Festplatte".into(),
            Some(false) if d.name.starts_with("nvme") => "NVMe".into(),
            Some(false) => "ohne bewegte Teile".into(),
            None => tx::NONE.into(),
        }));
        items.push(Item::Row("Gelesen seit dem Start".into(), c.m(tx::bytes(d.read_total))));
        items.push(Item::Row("Geschrieben seit dem Start".into(), c.m(tx::bytes(d.write_total))));
        if !d.parts.is_empty() {
            items.push(Item::Row("Teile".into(), d.parts.iter().map(|(n, sz)| format!("{n} {}", tx::bytes(Some(*sz)))).collect::<Vec<_>>().join(" · ")));
        }
        if d.name.starts_with("nvme") {
            items.push(Item::Row("Temperatur".into(), c.m(tx::celsius(s.heat.nvme))));
            items.push(Item::Row("Abgenutzt".into(), tx::NONE.into()));
        }
        devs.push(card(1, items));
    }
    if devs.is_empty() {
        devs.push(card(1, vec![Item::Label("Datenträger".into()), Item::Sub("Der Kern meldet keine Datenträger.".into())]));
    }
    devs.truncate(4);
    TabModel {
        sub: format!("{} Datenträger", s.disks.len()),
        rows: vec![row(vec![card(1, list), chart], false), row(devs, true)],
        ..TabModel::default()
    }
}

pub fn net(c: &Ctx) -> TabModel {
    let s = c.s;
    let (rx, txr, tot, main) = net_now(c);
    let chart = card(2, vec![
        Item::LabelRight(if main.is_empty() { "Alle Anschlüsse".into() } else { format!("Alle Anschlüsse · meist {main}") }, vec![(Tone::Amber, "Empfangen".into()), (Tone::Accent, "Gesendet".into())]),
        Item::Big(format!("↓ {}", c.m(tx::rate(rx))), format!("↑ {}", c.m(tx::rate(txr)))),
        c.chart(vec![("net.rx", Tone::Amber, true), ("net.tx", Tone::Accent, false)], None, 220),
    ]);
    let sent: u64 = s.nets.iter().map(|n| n.tx_total).sum();
    let totals = card(1, vec![
        Item::Label("Seit dem Start".into()),
        Item::Row("Empfangen".into(), c.m(tx::bytes(Some(tot)))),
        Item::Row("Gesendet".into(), c.m(tx::bytes(Some(sent)))),
        Item::Rule,
        Item::Sub("Welches Programm wie viel schickt, misst BitsOS noch nicht; wohin es sich verbindet, steht unter „Wer hat was gemacht“.".into()),
    ]);
    let mut ifs = Vec::new();
    for n in &s.nets {
        let kind = if n.wifi.is_some() || n.name.starts_with("wl") { "WLAN" } else { "Kabel" };
        let (state, tone) = match n.up {
            Some(true) => ("verbunden", Tone::Accent),
            Some(false) => (if kind == "WLAN" { "nicht verbunden" } else { "kein Kabel" }, Tone::Muted),
            None => ("–", Tone::Muted),
        };
        let mut items = vec![Item::Label(format!("{kind} · {}", n.name)), Item::RowTone("Zustand".into(), state.into(), tone)];
        if let Some(sp) = n.speed_mbps {
            items.push(Item::Row("Geschwindigkeit".into(), if sp >= 1000 { format!("{} Gbit/s", tx::dec(sp as f64 / 1000.0, 1)) } else { format!("{sp} Mbit/s") }));
        }
        items.push(Item::Row("Jetzt".into(), c.m(format!("↓ {} · ↑ {}", tx::rate(n.rx), tx::rate(n.tx)))));
        if let Some((q, dbm)) = n.wifi {
            items.push(Item::Row("Empfang".into(), c.m(format!("{} · {} dBm", tx::dec(q, 0), tx::dec(dbm, 0)))));
        }
        ifs.push(card(1, items));
    }
    if ifs.is_empty() {
        ifs.push(card(1, vec![Item::Label("Anschlüsse".into()), Item::Sub("Der Kern meldet keine Netzanschlüsse.".into())]));
    }
    ifs.truncate(4);
    TabModel { sub: format!("{} Anschlüsse", s.nets.len()), rows: vec![row(vec![chart, totals], false), row(ifs, true)], ..TabModel::default() }
}

pub fn power(c: &Ctx) -> TabModel {
    let s = c.s;
    let h = &s.heat;
    let temps = card(2, vec![
        Item::LabelRight("Temperaturen".into(), vec![(Tone::Accent, "Prozessor".into()), (Tone::Blue, "Grafik".into()), (Tone::Amber, "NVMe".into())]),
        Item::Big(c.m(tx::celsius(h.cpu)), heat_words(c)),
        c.chart(vec![("temp.cpu", Tone::Accent, false), ("temp.gpu", Tone::Blue, false), ("temp.nvme", Tone::Amber, false)], Some(100.0), 200),
    ]);
    let now = card(1, vec![
        Item::Label("Jetzt".into()),
        Item::Grid(vec![
            ("Prozessor".into(), c.m(tx::celsius(h.cpu))),
            ("Grafik".into(), c.m(tx::celsius(h.gpu))),
            ("NVMe".into(), c.m(tx::celsius(h.nvme))),
            ("Prozessor-Paket".into(), c.m(tx::watt(h.cpu_power))),
            ("Grafik".into(), c.m(tx::watt(h.gpu_power))),
            ("Zusammen".into(), c.m(tx::watt(h.cpu_power.zip(h.gpu_power).map(|(a, b)| a + b)))),
        ]),
        Item::Sub(if h.cpu_power.is_none() { tx::NO_SOURCE.into() } else { "Gemittelt über einige Sekunden.".into() }),
    ]);
    let watts = card(2, vec![
        Item::LabelRight("Strom".into(), vec![(Tone::Accent, "Prozessor-Paket".into()), (Tone::Blue, "Grafik".into())]),
        c.chart(vec![("power.cpu", Tone::Accent, true), ("power.gpu", Tone::Blue, false)], None, 160),
    ]);
    let mut all = vec![Item::Label("Alle Fühler".into())];
    if c.v.mask {
        all.push(Item::Row(MASK.into(), MASK.into()));
    } else {
        all.extend(h.all.iter().take(10).map(|(n, l, t)| Item::Row(format!("{n} · {l}"), tx::celsius(Some(*t)))));
    }
    if h.all.is_empty() {
        all.push(Item::Sub(tx::NO_SOURCE.into()));
    }
    TabModel { sub: format!("{} Fühler", h.all.len()), rows: vec![row(vec![temps, now], false), row(vec![watts, card(1, all)], true)], ..TabModel::default() }
}

pub fn history(c: &Ctx) -> TabModel {
    let since = c.h.since.map(|t| c.s.wall - (c.s.t.saturating_sub(t)) as i64);
    let opened = since.map_or_else(|| "seit das Programm offen ist".to_string(), |w| format!("seit das Programm offen ist ({} Uhr)", tx::clock(w, c.off_min)));
    let source = match (c.v.range, c.journal) {
        (Range::Day, Some(true)) => format!("Prozessor, Temperatur und Netz der letzten 24 Stunden aus dem Protokoll von BitsOS; der Arbeitsspeicher {opened}."),
        (Range::Day, Some(false)) => format!("Alle Werte {opened}: das Protokoll von BitsOS ist für den Desktop nicht lesbar."),
        (Range::Day, None) => format!("Alle Werte {opened}; das Protokoll von BitsOS wird gefragt."),
        _ => format!("Gemessen {opened}, einmal je Sekunde."),
    };
    let mk = |label: &str, series: Vec<(&str, Tone, bool)>, legend: Vec<(Tone, String)>, max: Option<f64>| card(1, vec![Item::LabelRight(label.into(), legend), c.chart(series, max, 150)]);
    let rows = vec![
        row(vec![
            mk("Prozessor", vec![("cpu", Tone::Accent, true)], Vec::new(), Some(100.0)),
            mk("Arbeitsspeicher", vec![("mem.used", Tone::Accent, true)], Vec::new(), c.s.mem.mem.map(|m| m.total as f64)),
        ], false),
        row(vec![
            mk("Temperatur", vec![("temp.cpu", Tone::Accent, false), ("temp.gpu", Tone::Blue, false)], vec![(Tone::Accent, "Prozessor".into()), (Tone::Blue, "Grafik".into())], Some(100.0)),
            mk("Netz", vec![("net.rx", Tone::Amber, true), ("net.tx", Tone::Accent, false)], vec![(Tone::Amber, "Empfangen".into()), (Tone::Accent, "Gesendet".into())], None),
        ], false),
        row(vec![card(1, vec![Item::Label("Woher die Werte kommen".into()), Item::Sub(source)])], false),
    ];
    TabModel { sub: c.v.range.name().into(), rows, ..TabModel::default() }
}
