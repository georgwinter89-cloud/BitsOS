//! Resampler measurements (host, f64 analysis): THD+N by a least-squares sine fit, image rejection, chunking
//! invariance, exact unity at DC. `cargo test -p snd -- --nocapture thd` prints the numbers for the notes.
extern crate std;

use crate::resample::{Resampler, Table, table, RATES};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use std::println;

fn sine(rate: u32, f: f64, amp: f64, n: usize) -> Vec<[i16; 2]> {
    (0..n)
        .map(|i| {
            let s = (amp * (2.0 * core::f64::consts::PI * f * i as f64 / rate as f64).sin()).round() as i16;
            [s, s]
        })
        .collect()
}

fn run(t: Arc<Table>, input: &[[i16; 2]], outs: usize) -> Vec<[i32; 2]> {
    let mut r = Resampler::try_new(t).unwrap();
    let need = r.inputs_for(outs);
    let mut inp = input.to_vec();
    inp.resize(need, [0; 2]);
    let mut out = vec![[0; 2]; outs];
    r.run(&inp, &mut out);
    out
}

/// Fits a*sin + b*cos at `f` (48 kHz) over `x`; returns (amplitude, THD+N in dB).
fn fit(x: &[f64], f: f64) -> (f64, f64) {
    let w = 2.0 * core::f64::consts::PI * f / 48000.0;
    let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for (i, v) in x.iter().enumerate() {
        let (s, c) = ((w * i as f64).sin(), (w * i as f64).cos());
        ss += s * s;
        cc += c * c;
        sc += s * c;
        xs += v * s;
        xc += v * c;
    }
    let det = ss * cc - sc * sc;
    let a = (xs * cc - xc * sc) / det;
    let b = (xc * ss - xs * sc) / det;
    let mut res = 0.0;
    let mut sig = 0.0;
    for (i, v) in x.iter().enumerate() {
        let m = a * (w * i as f64).sin() + b * (w * i as f64).cos();
        res += (v - m) * (v - m);
        sig += m * m;
    }
    ((a * a + b * b).sqrt(), 10.0 * (res / sig).log10())
}

fn thd(rate: u32, f: f64) -> (f64, f64) {
    let n_in = rate as usize; // 1 s
    let out = run(table(rate).unwrap(), &sine(rate, f, 29205.0, n_in), 48000);
    // Skip the filter's start, analyse whole cycles of the steady part.
    let x: Vec<f64> = out[2000..2000 + 38400].iter().map(|s| s[0] as f64).collect();
    fit(&x, f)
}

#[test]
fn thd_44k1() {
    for f in [1000.0, 5000.0, 10000.0, 15000.0] {
        let (amp, d) = thd(44100, f);
        println!("thd 44.1k->48k {f} Hz at -1 dBFS: amplitude {amp:.1} (in 29205) THD+N {d:.1} dB");
        assert!(d < -85.0, "{f} Hz: THD+N {d:.1} dB");
        assert!((amp / 29205.0 - 1.0).abs() < 0.005, "{f} Hz: gain {}", amp / 29205.0);
    }
}

#[test]
fn thd_other_rates() {
    for rate in [8000u32, 22050, 32000, 96000, 192000] {
        let (amp, d) = thd(rate, 1000.0);
        println!("thd {rate}->48k 1 kHz: amplitude {amp:.1} THD+N {d:.1} dB");
        assert!(d < -85.0 && (amp / 29205.0 - 1.0).abs() < 0.005, "{rate}: {d} {amp}");
    }
}

#[test]
fn image_rejection_44k1() {
    // A 20 kHz input images at 44.1 - 20 = 24.1 kHz, which lands at 23.9 kHz at 48 kHz.
    let out = run(table(44100).unwrap(), &sine(44100, 20000.0, 29205.0, 44100), 48000);
    let x: Vec<f64> = out[2000..2000 + 38400].iter().map(|s| s[0] as f64).collect();
    let (want, _) = fit(&x, 20000.0);
    let (image, _) = fit(&x, 23900.0);
    let db = 20.0 * (image / want).log10();
    println!("image 23.9 kHz vs 20 kHz: {db:.1} dB; 20 kHz passes at {:.2} dB", 20.0 * (want / 29205.0).log10());
    assert!(db < -80.0, "image {db:.1} dB");
    // An audible image: 15 kHz images at 29.1 kHz, which folds to 18.9 kHz.
    let out = run(table(44100).unwrap(), &sine(44100, 15000.0, 29205.0, 44100), 48000);
    let x: Vec<f64> = out[2000..2000 + 38400].iter().map(|s| s[0] as f64).collect();
    let (want, _) = fit(&x, 15000.0);
    let (image, _) = fit(&x, 18900.0);
    let db = 20.0 * (image / want).log10();
    println!("image 18.9 kHz vs 15 kHz: {db:.1} dB");
    assert!(db < -85.0, "image {db:.1} dB");
    // Pass band: flat to 18 kHz within 0.1 dB, 19 kHz within 0.5 dB.
    for (f, tol) in [(18000.0, 0.1), (19000.0, 0.5)] {
        let out = run(table(44100).unwrap(), &sine(44100, f, 29205.0, 44100), 48000);
        let x: Vec<f64> = out[2000..2000 + 38400].iter().map(|s| s[0] as f64).collect();
        let g = 20.0 * (fit(&x, f).0 / 29205.0).log10();
        println!("pass band {f} Hz: {g:.3} dB");
        assert!(g.abs() < tol, "{f} Hz at {g:.3} dB");
    }
}

#[test]
fn chunking_does_not_matter() {
    let input = sine(44100, 997.0, 20000.0, 9000);
    let whole = run(table(44100).unwrap(), &input, 9000);
    let mut r = Resampler::try_new(table(44100).unwrap()).unwrap();
    let mut got: Vec<[i32; 2]> = Vec::new();
    let mut at = 0;
    let mut k = 1;
    while got.len() < 9000 {
        let n = (k * 37 % 500 + 1).min(9000 - got.len());
        let need = r.inputs_for(n);
        let mut inp: Vec<[i16; 2]> = input[at.min(input.len())..(at + need).min(input.len())].to_vec();
        inp.resize(need, [0; 2]);
        at += need;
        let mut out = vec![[0; 2]; n];
        r.run(&inp, &mut out);
        got.extend(out);
        k += 1;
    }
    assert_eq!(got, whole);
}

#[test]
fn dc_is_exact_and_full_scale_clamps() {
    for rate in RATES {
        if rate == 48000 {
            continue;
        }
        let t = Table::build(rate).unwrap();
        for p in 0..t.l as usize {
            let s: i64 = t.coef[p * t.taps..(p + 1) * t.taps].iter().map(|&c| c as i64).sum();
            assert_eq!(s, 1 << crate::resample::COEF_SHIFT, "rate {rate} phase {p}");
        }
        let out = run(Arc::new(t), &vec![[12345, -32768]; 8000], 1500);
        assert!(out[500..].iter().all(|s| *s == [12345, -32768]), "rate {rate}");
    }
    // A full-scale square overshoots: clamped, no wrap.
    let sq: Vec<[i16; 2]> = (0..4410).map(|i| if (i / 50) % 2 == 0 { [32767, 32767] } else { [-32768, -32768] }).collect();
    let out = run(table(44100).unwrap(), &sq, 4800);
    assert!(out.iter().all(|s| (-32768..=32767).contains(&s[0])));
}

#[test]
fn table_shape() {
    let t = Table::build(44100).unwrap();
    assert_eq!((t.l, t.m, t.taps), (160, 147, 64));
    let t = Table::build(96000).unwrap();
    assert_eq!((t.l, t.m, t.taps), (1, 2, 128));
}

/// `outputs_for` is the exact inverse of `inputs_for` at every filter phase (the mixer tops up by it, SN-7).
#[test]
fn outputs_for_inverts_inputs_for() {
    for &rate in RATES.iter().filter(|&&r| r != 48000) {
        let mut r = Resampler::try_new(table(rate).unwrap()).unwrap();
        let mut out = vec![[0i32; 2]; 97];
        for step in 0..40 {
            for avail in 0..600 {
                let n = r.outputs_for(avail);
                assert!(r.inputs_for(n) <= avail && r.inputs_for(n + 1) > avail, "rate {rate} step {step} avail {avail} n {n}");
            }
            let k = 1 + step % 7 * 13;
            let inp = vec![[0i16; 2]; r.inputs_for(k)];
            r.run(&inp, &mut out[..k]);
        }
    }
}
