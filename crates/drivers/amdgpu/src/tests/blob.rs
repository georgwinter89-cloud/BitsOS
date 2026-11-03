//! Synthetic IP discovery binaries for the host tests: the layout of linux-6.6 include/discovery.h, written by
//! BitsOS's own builder (header, IP table with its dies, then GC, harvest, VCN, MALL), with every checksum right.
//! Tests then break one thing at a time.

use crate::gpucore::disc::{self, checksum};
use std::vec;
use std::vec::Vec;

#[derive(Clone)]
pub struct Ip {
    pub hw_id: u16,
    pub inst: u8,
    pub ver: (u8, u8, u8),
    pub byte7: u8,
    pub bases: Vec<u64>,
}

pub fn ip(hw_id: u16, inst: u8, ver: (u8, u8, u8), bases: &[u64]) -> Ip {
    Ip { hw_id, inst, ver, byte7: 0, bases: bases.to_vec() }
}

#[derive(Clone)]
pub struct Blob {
    pub table_version: u16,
    pub bases_64: bool,
    pub dies: Vec<Vec<Ip>>,
    /// (major, minor, field values).
    pub gc: Option<(u16, u16, Vec<u32>)>,
    pub harvest: Option<Vec<(u16, u8)>>,
    /// (instance, fuses) per VCN instance.
    pub vcn: Option<Vec<(u32, u32)>>,
    pub mall: Option<(u16, u16, Vec<u32>)>,
    /// The buffer length (the reserved region is 8 KiB).
    pub len: usize,
}

impl Blob {
    pub fn new(ips: Vec<Ip>) -> Blob {
        Blob { table_version: 4, bases_64: false, dies: vec![ips], gc: None, harvest: None, vcn: None, mall: None, len: disc::TMR_SIZE }
    }
}

/// Byte offsets of the parts, for tests that break them.
#[derive(Default, Debug, Clone, Copy)]
pub struct Layout {
    pub ip: usize,
    pub die0: usize,
    pub gc: usize,
    pub harvest: usize,
    pub vcn: usize,
    pub mall: usize,
    pub size: usize,
}

fn put16(b: &mut Vec<u8>, v: u16) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut Vec<u8>, v: u32) {
    b.extend_from_slice(&v.to_le_bytes());
}
fn set16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}
fn set32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}
fn align4(b: &mut Vec<u8>) {
    while b.len() % 4 != 0 {
        b.push(0);
    }
}

/// table_list slot of a table (IP=0, GC=1, harvest=2, VCN=3, MALL=4).
pub fn slot(t: usize) -> usize {
    12 + t * 8
}

pub fn build(bl: &Blob) -> (Vec<u8>, Layout) {
    let mut b = vec![0u8; 60];
    set32(&mut b, 0, disc::BINARY_SIGNATURE);
    set16(&mut b, 4, 1);
    set16(&mut b, 6, 0);
    align4(&mut b);
    let mut l = Layout { ip: b.len(), ..Layout::default() };
    // IP discovery header.
    put32(&mut b, disc::IP_TABLE_SIGNATURE);
    put16(&mut b, bl.table_version);
    put16(&mut b, 0); // size, set below
    put32(&mut b, 0);
    put16(&mut b, bl.dies.len() as u16);
    let die_info = b.len();
    b.resize(die_info + 64, 0);
    b.push(u8::from(bl.bases_64));
    b.push(0);
    for (i, die) in bl.dies.iter().enumerate() {
        let at = b.len();
        if i == 0 {
            l.die0 = at;
        }
        set16(&mut b, die_info + i * 4, i as u16);
        set16(&mut b, die_info + i * 4 + 2, at as u16);
        put16(&mut b, i as u16);
        put16(&mut b, die.len() as u16);
        for e in die {
            put16(&mut b, e.hw_id);
            b.extend_from_slice(&[e.inst, e.bases.len() as u8, e.ver.0, e.ver.1, e.ver.2, e.byte7]);
            for &a in &e.bases {
                if bl.bases_64 {
                    b.extend_from_slice(&a.to_le_bytes());
                } else {
                    put32(&mut b, a as u32);
                }
            }
        }
    }
    let ip_size = b.len() - l.ip;
    set16(&mut b, l.ip + 6, ip_size as u16);
    let mut tables: Vec<(usize, usize, usize)> = vec![(0, l.ip, ip_size)];
    align4(&mut b);
    if let Some((major, minor, fields)) = &bl.gc {
        l.gc = b.len();
        put32(&mut b, disc::GC_TABLE_ID);
        put16(&mut b, *major);
        put16(&mut b, *minor);
        put32(&mut b, (12 + fields.len() * 4) as u32);
        fields.iter().for_each(|&f| put32(&mut b, f));
        tables.push((1, l.gc, b.len() - l.gc));
    }
    if let Some(h) = &bl.harvest {
        l.harvest = b.len();
        put32(&mut b, disc::HARVEST_SIGNATURE);
        put32(&mut b, 0);
        for i in 0..32 {
            let (id, inst) = h.get(i).copied().unwrap_or((0, 0));
            put16(&mut b, id);
            b.extend_from_slice(&[inst, 0]);
        }
        tables.push((2, l.harvest, 136));
    }
    if let Some(v) = &bl.vcn {
        l.vcn = b.len();
        put32(&mut b, disc::VCN_TABLE_ID);
        put16(&mut b, 1);
        put16(&mut b, 0);
        put32(&mut b, 12 + 4 + 64 + 16);
        put32(&mut b, v.len() as u32);
        for i in 0..4 {
            let (inst, fuses) = v.get(i).copied().unwrap_or((0, 0));
            put32(&mut b, inst);
            put32(&mut b, fuses);
            put32(&mut b, 0);
            put32(&mut b, 0);
        }
        b.resize(b.len() + 16, 0);
        tables.push((3, l.vcn, b.len() - l.vcn));
    }
    if let Some((major, minor, fields)) = &bl.mall {
        l.mall = b.len();
        put32(&mut b, disc::MALL_TABLE_ID);
        put16(&mut b, *major);
        put16(&mut b, *minor);
        put32(&mut b, (12 + fields.len() * 4) as u32);
        fields.iter().for_each(|&f| put32(&mut b, f));
        tables.push((4, l.mall, b.len() - l.mall));
    }
    l.size = b.len();
    set16(&mut b, 10, l.size as u16);
    for &(t, at, size) in &tables {
        set16(&mut b, slot(t), at as u16);
        let sum = checksum(&b[at..at + size]);
        set16(&mut b, slot(t) + 2, sum);
        set16(&mut b, slot(t) + 4, size as u16);
    }
    fix(&mut b);
    b.resize(bl.len.max(b.len()), 0);
    (b, l)
}

/// Recomputes the binary checksum (after a test changed bytes on purpose).
pub fn fix(b: &mut [u8]) {
    let size = usize::from(u16::from_le_bytes([b[10], b[11]])).min(b.len());
    let s = checksum(&b[10..size]);
    set16(b, 8, s);
}

/// Recomputes table `t`'s checksum over `len` bytes, then the binary's.
pub fn fix_table(b: &mut [u8], t: usize, len: usize) {
    let at = usize::from(u16::from_le_bytes([b[slot(t)], b[slot(t) + 1]]));
    let s = checksum(&b[at..at + len]);
    set16(b, slot(t) + 2, s);
    fix(b);
}

pub fn poke16(b: &mut [u8], at: usize, v: u16) {
    set16(b, at, v);
}
pub fn poke32(b: &mut [u8], at: usize, v: u32) {
    set32(b, at, v);
}

/// The 780M's IP list from tests/data/minipc-780m-ip.txt (Linux's sysfs view of the Mini-PC, 27.10.).
pub fn minipc_ips() -> Vec<Ip> {
    let text = include_str!("../../tests/data/minipc-780m-ip.txt");
    text.lines()
        .filter(|l| l.starts_with("ip "))
        .map(|l| {
            let t: Vec<&str> = l.split_whitespace().collect();
            let ver: Vec<u8> = t[4].split('.').map(|x| x.parse().unwrap()).collect();
            let bases = t.iter().skip_while(|&&x| x != "bases").skip(1).map(|x| u64::from_str_radix(&x[2..], 16).unwrap()).collect();
            Ip { hw_id: t[2].parse().unwrap(), inst: t[3].parse().unwrap(), ver: (ver[0], ver[1], ver[2]), byte7: 0, bases }
        })
        .collect()
}
