//! Register names for BAR5 dwords from the generated tables (tables/*.tsv) and an IP discovery's bases: shared by
//! the `amdtrace` and `regtrace` tools (included by #[path]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which table names an IP's registers: (hw_id, major, minor, table). Linux 6.6 builds each block against
/// these headers: gfx_v11_0/sdma_v6_0/gfxhub_v3_0 include gc_11_0_0 (no gc_11_0_1 header exists), ih_v6_0
/// osssys_6_0_0, psp_v13_0_4 mp_13_0_4, mmhub_v3_0_1, nbio_v7_7, vcn_v4_0 vcn_4_0_0, hdp_v5_2 hdp_5_2_1,
/// lsdma_v6_0, gfx_v11_0 smuio_13_0_6.
pub const MAP: &[(u16, u8, u8, &str)] = &[
    (11, 11, 0, "gc_11_0_0"),
    (255, 13, 0, "mp_13_0_4"),
    (108, 7, 7, "nbio_7_7_0"),
    (34, 3, 0, "mmhub_3_0_1"),
    (40, 6, 0, "osssys_6_0_0"),
    (41, 5, 2, "hdp_5_2_1"),
    (12, 4, 0, "vcn_4_0_0"),
    (91, 6, 0, "lsdma_6_0_0"),
    (4, 13, 0, "smuio_13_0_6"),
    (271, 3, 1, "dcn_3_1_4"),
];
/// MP1's registers live in MP0's header; they resolve against MP1's bases.
pub const MP1: u16 = 1;
/// IPs whose registers another IP's table names (MP1 in mp_13_0_4, SDMA0 in gc_11_0_0 at GC's bases).
pub const COVERED: &[u16] = &[MP1, 42];

/// One IP instance from discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ip {
    pub hw_id: u16,
    pub inst: u8,
    pub ver: (u8, u8, u8),
    pub bases: Vec<u32>,
}

pub fn hex(s: &str) -> Result<u64, String> {
    let t = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    u64::from_str_radix(t, 16).map_err(|e| format!("bad hex {s:?}: {e}"))
}

fn version(s: &str) -> Result<(u8, u8, u8), String> {
    let p: Vec<&str> = s.split('.').collect();
    let n = |i: usize| p.get(i).and_then(|x| x.parse().ok()).ok_or(format!("bad version {s:?}"));
    Ok((n(0)?, n(1)?, n(2)?))
}

/// `ip <name> <hw_id> <inst> <ver> ... bases 0x...` lines; anything else is ignored.
pub fn bases_from_lines(text: &str) -> Result<Vec<Ip>, String> {
    let mut v = Vec::new();
    for (ln, l) in text.lines().enumerate() {
        let t: Vec<&str> = l.split_whitespace().collect();
        if t.first() != Some(&"ip") || t.len() < 5 {
            continue;
        }
        let at = |e: String| format!("line {}: {e}", ln + 1);
        let hw_id = t[2].parse().map_err(|_| at(format!("hw_id {:?}", t[2])))?;
        let inst = t[3].parse().map_err(|_| at(format!("instance {:?}", t[3])))?;
        let ver = version(t[4]).map_err(at)?;
        let bases = t
            .iter()
            .skip_while(|&&x| x != "bases")
            .skip(1)
            .map(|x| hex(x).map(|b| b as u32))
            .collect::<Result<_, _>>()
            .map_err(at)?;
        v.push(Ip { hw_id, inst, ver, bases });
    }
    Ok(v)
}

/// Linux's sysfs tree ip_discovery/die/0/<hw_id>/<instance>/{major,minor,revision,base_addr}.
pub fn bases_from_sysfs(dir: &Path) -> Result<Vec<Ip>, String> {
    let rd = |p: PathBuf| std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()));
    let mut v = Vec::new();
    let mut ids: Vec<u16> = std::fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    ids.sort();
    for id in ids {
        let d = dir.join(id.to_string());
        let mut insts: Vec<u8> =
            std::fs::read_dir(&d).map_err(|e| e.to_string())?.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).collect();
        insts.sort();
        for i in insts {
            let p = d.join(i.to_string());
            let n = |f: &str| -> Result<u8, String> { rd(p.join(f))?.trim().parse().map_err(|_| format!("{}/{f}", p.display())) };
            let bases = rd(p.join("base_addr"))?.split_whitespace().map(|x| hex(x).map(|b| b as u32)).collect::<Result<_, _>>()?;
            v.push(Ip { hw_id: id, inst: i, ver: (n("major")?, n("minor")?, n("revision")?), bases });
        }
    }
    Ok(v)
}

/// The table rows: (name, base_idx, dword).
pub fn read_table(text: &str) -> Result<Vec<(String, usize, u32)>, String> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let t: Vec<&str> = l.split('\t').collect();
            match t[..] {
                [n, i, d] => Ok((n.to_string(), i.parse().map_err(|_| format!("bad row {l:?}"))?, hex(d)? as u32)),
                _ => Err(format!("bad row {l:?}")),
            }
        })
        .collect()
}

/// BAR5 dword -> names, from the tables that MAP assigns to the discovered IPs.
pub struct Names {
    pub by_dword: BTreeMap<u32, Vec<String>>,
    pub unmapped: Vec<String>,
}

pub fn ip_label(ip: &Ip) -> String {
    format!("hw{}.{} v{}.{}.{}", ip.hw_id, ip.inst, ip.ver.0, ip.ver.1, ip.ver.2)
}

pub fn names(ips: &[Ip], table: impl Fn(&str) -> Result<String, String>) -> Result<Names, String> {
    let mut by_dword: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    let mut unmapped = Vec::new();
    let mut cache: BTreeMap<&str, Vec<(String, usize, u32)>> = BTreeMap::new();
    for ip in ips {
        let Some(&(_, _, _, t)) = MAP.iter().find(|&&(h, ma, mi, _)| h == ip.hw_id && ma == ip.ver.0 && mi == ip.ver.1) else {
            if !COVERED.contains(&ip.hw_id) {
                unmapped.push(ip_label(ip));
            }
            continue;
        };
        if !cache.contains_key(t) {
            cache.insert(t, read_table(&table(t)?)?);
        }
        let mp1 = ips.iter().find(|x| x.hw_id == MP1 && x.inst == ip.inst);
        for (n, idx, dw) in &cache[t] {
            // MP1's registers in MP0's header use MP1's bases.
            let owner = if t.starts_with("mp_") && n.starts_with("MP1_") { mp1.unwrap_or(ip) } else { ip };
            let Some(base) = owner.bases.get(*idx) else { continue };
            let suffix = if ip.inst == 0 { String::new() } else { format!("#{}", ip.inst) };
            let list = by_dword.entry(base.wrapping_add(*dw)).or_default();
            let name = format!("{n}{suffix}");
            if !list.contains(&name) {
                list.push(name);
            }
        }
    }
    Ok(Names { by_dword, unmapped })
}

