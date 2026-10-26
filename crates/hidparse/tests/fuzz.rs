//! Hostile input: random bytes, item soup with extreme values and mutations of real descriptors go
//! through the parser, the mapping, value reading and report processing. Nothing may panic.

mod common;

use common::descs;
use hidparse::boot::{BOOT_KEYBOARD, BOOT_MOUSE};
use hidparse::{InputMap, InputState, parse};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
    /// Up to `max` random bytes.
    fn some_bytes(&mut self, max: u64) -> Vec<u8> {
        let n = self.below(max + 1) as usize;
        self.bytes(n)
    }
}

fn item(r: &mut Rng) -> Vec<u8> {
    let extreme = [
        0u32,
        1,
        0x7F,
        0x80,
        0xFF,
        0xFFFF,
        0x8000_0000,
        0xFFFF_FFFF,
        0x0009_0001,
        0x0001_0030,
    ];
    let prefix = match r.below(4) {
        // Mostly real tags: main, global, local, with any size.
        0 => [0x80u8, 0x90, 0xA0, 0xB0, 0xC0][r.below(5) as usize] | r.below(4) as u8,
        1 => (r.below(12) as u8) << 4 | 0x04 | r.below(4) as u8,
        2 => [0x08u8, 0x18, 0x28, 0xA8][r.below(4) as usize] | r.below(4) as u8,
        _ => r.next() as u8,
    };
    let size = [0usize, 1, 2, 4][(prefix & 3) as usize];
    let v = if r.below(2) == 0 {
        extreme[r.below(extreme.len() as u64) as usize]
    } else {
        r.next() as u32
    };
    let mut out = vec![prefix];
    out.extend_from_slice(&v.to_le_bytes()[..size]);
    if prefix == 0xFE {
        let n = r.below(6) as u8;
        out = vec![0xFE, n, r.next() as u8];
        out.extend(r.bytes(n as usize));
    }
    out
}

fn mutate(r: &mut Rng, base: &[u8]) -> Vec<u8> {
    let mut b = base.to_vec();
    for _ in 0..1 + r.below(4) {
        let at = r.below(b.len() as u64 + 1) as usize;
        match r.below(6) {
            0 | 1 if !b.is_empty() => {
                let at = at % b.len();
                b[at] = r.next() as u8;
            }
            2 => b.insert(at, r.next() as u8),
            3 if at < b.len() => {
                b.remove(at);
            }
            4 => {
                let it = item(r);
                b.splice(at..at, it);
            }
            _ => b.truncate(at),
        }
    }
    b
}

fn exercise(r: &mut Rng, desc: &[u8]) {
    let Ok(d) = parse(desc) else { return };
    for rep in &d.reports {
        for f in &rep.fields {
            let data = r.some_bytes(40);
            for n in 0..f.count.min(3) {
                let _ = f.value(&data, n);
            }
            let _ = f.usage(r.next());
        }
    }
    let map = InputMap::new(&d);
    let max = r.below(300) as usize;
    let mut state = InputState::new(&d, &map, max);
    let mut out = Vec::new();
    for _ in 0..4 {
        let mut report = r.some_bytes(max as u64 + 2);
        if let (Some(first), Some(rep)) = (
            report.first_mut(),
            d.reports.get(r.below(d.reports.len() as u64) as usize),
        ) {
            *first = rep.id;
        }
        let _ = state.process(&d, &map, &report, &mut out);
        out.clear();
    }
}

#[test]
fn hostile_descriptors_and_reports_never_panic() {
    let seeds = [
        BOOT_KEYBOARD.to_vec(),
        BOOT_MOUSE.to_vec(),
        descs::dragonrise(),
        descs::dualshock4(),
        descs::dualshock3(),
        descs::snes_clone(),
        descs::keyboard_and_pad(),
    ];
    let mut r = Rng(0x1D_5EED_2026_1014);
    let mut parsed = 0;
    for i in 0..60_000u32 {
        let desc = match i % 4 {
            0 => r.some_bytes(80),
            1 => (0..1 + r.below(30)).flat_map(|_| item(&mut r)).collect(),
            _ => {
                let base = &seeds[r.below(seeds.len() as u64) as usize];
                mutate(&mut r, base)
            }
        };
        parsed += parse(&desc).is_ok() as u32;
        exercise(&mut r, &desc);
    }
    eprintln!("fuzz: {parsed} of 60000 descriptors parsed");
    assert!(
        parsed > 10_000,
        "too few descriptors got past the parser to test the rest"
    );
}

/// Parses, maps and feeds one report; returns how long it took.
fn timed(desc: &[u8]) -> std::time::Duration {
    let t = std::time::Instant::now();
    let d = parse(desc).unwrap();
    let map = InputMap::new(&d);
    let mut state = InputState::new(&d, &map, 64);
    let mut out = Vec::new();
    let id = d.reports.first().map_or(0, |r| r.id);
    let _ = state.process(&d, &map, &[id; 64], &mut out);
    if let Some(f) = d.reports.first().and_then(|r| r.fields.first()) {
        for c in 0..d.collections.len() {
            let _ = f.is_inside(&d.collections, c);
        }
    }
    t.elapsed()
}

#[test]
fn large_hostile_shapes_stay_linear() {
    // About 96 KB each: work must stay linear in the length (debug build budget: 2 s each).
    let n = 16_000;
    // Deep nesting: 16,000 collections, a field inside each level.
    let mut deep = Vec::new();
    for _ in 0..n {
        deep.extend_from_slice(&[0x09, 0x01, 0xA1, 0x00, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02]);
    }
    deep.extend(std::iter::repeat_n(0xC0, n));
    // Many fields, each declaring a whole consumer page (65,536 usages).
    let mut wide = vec![
        0x05, 0x0C, 0x75, 0x10, 0x95, 0x01, 0x15, 0x00, 0x27, 0xFF, 0xFF, 0, 0,
    ];
    for _ in 0..n * 6 / 7 {
        wide.extend_from_slice(&[0x19, 0x00, 0x2A, 0xFF, 0xFF, 0x81, 0x00]);
    }
    // One field with thousands of whole-page ranges.
    let mut spans = wide[..13].to_vec();
    for _ in 0..n * 6 / 5 {
        spans.extend_from_slice(&[0x19, 0x00, 0x2A, 0xFF, 0xFF]);
    }
    spans.extend_from_slice(&[0x81, 0x00]);
    for (name, desc) in [("deep", deep), ("wide", wide), ("spans", spans)] {
        let took = timed(&desc);
        eprintln!("{name}: {} bytes in {took:?}", desc.len());
        assert!(took.as_secs_f64() < 2.0, "{name} took {took:?}");
    }
}

#[test]
fn extreme_caller_values_saturate() {
    // A value index past the field, behind a report of almost 2^64 bits.
    let d = parse(&[
        0x77, 0xFF, 0xFF, 0xFF, 0xFF, 0x97, 0xFF, 0xFF, 0xFF, 0xFF, 0x81, 0x02, 0x95, 0x01, 0x81,
        0x02,
    ])
    .unwrap();
    let f = &d.reports[0].fields[1];
    assert_eq!(f.value_offset(2), u64::MAX);
    assert_eq!(f.value_offset(u32::MAX), u64::MAX);
    assert!(f.value(&[0; 8], u32::MAX).is_err());
    // The largest packet size a caller can pass.
    let d = parse(BOOT_KEYBOARD).unwrap();
    let map = InputMap::new(&d);
    let mut state = InputState::new(&d, &map, usize::MAX);
    let mut out = Vec::new();
    state
        .process(&d, &map, &[0, 0, 4, 0, 0, 0, 0, 0], &mut out)
        .unwrap();
    assert!(!out.is_empty());
}
