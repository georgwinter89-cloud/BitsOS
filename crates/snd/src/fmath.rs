//! The few f64 functions the filter design needs, without libm (the kernel is `no_std`). The kernel target is
//! soft-float, so these compile to integer code (R10: no SSE in the kernel); they run once per sample-rate
//! ratio when a client opens, never in the mixing path (that is integer only).

use core::f64::consts::PI;

pub fn floor(x: f64) -> f64 {
    let i = x as i64 as f64;
    if i > x { i - 1.0 } else { i }
}

/// sin(pi * x), about 1e-15 accurate.
pub fn sin_pi(x: f64) -> f64 {
    let mut r = x - 2.0 * floor(x / 2.0 + 0.5); // [-1, 1)
    if r > 0.5 {
        r = 1.0 - r;
    } else if r < -0.5 {
        r = -1.0 - r;
    }
    let t = PI * r; // |t| <= pi/2
    let t2 = t * t;
    let mut term = t;
    let mut sum = t;
    let mut k = 1.0;
    while k < 25.0 {
        term *= -t2 / ((k + 1.0) * (k + 2.0));
        sum += term;
        k += 2.0;
    }
    sum
}

/// sin(pi x) / (pi x).
pub fn sinc(x: f64) -> f64 {
    if x > -1e-12 && x < 1e-12 { 1.0 } else { sin_pi(x) / (PI * x) }
}

pub fn sqrt(x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    let mut g = if x > 1.0 { x } else { 1.0 };
    for _ in 0..200 {
        let n = 0.5 * (g + x / g);
        if n >= g {
            return n.min(g);
        }
        g = n;
    }
    g
}

/// Modified Bessel function of the first kind, order 0 (the Kaiser window).
pub fn bessel_i0(x: f64) -> f64 {
    let q = x * x / 4.0;
    let mut term = 1.0;
    let mut sum = 1.0;
    let mut k = 1.0;
    while k < 200.0 {
        term *= q / (k * k);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
        k += 1.0;
    }
    sum
}

#[cfg(test)]
mod tests {
    extern crate std;

    #[test]
    fn against_std() {
        for i in -400..400 {
            let x = i as f64 * 0.0137;
            assert!((super::sin_pi(x) - (core::f64::consts::PI * x).sin()).abs() < 1e-13, "{x}");
            let y = (i + 400) as f64 * 0.01;
            assert!((super::sqrt(y) - y.sqrt()).abs() < 1e-13, "{y}");
            assert_eq!(super::floor(x), x.floor());
        }
        // I0(1) and I0(9) from tables.
        assert!((super::bessel_i0(1.0) - 1.2660658777520082).abs() < 1e-14);
        assert!((super::bessel_i0(9.0) / 1093.588354511375 - 1.0).abs() < 1e-13);
    }
}
