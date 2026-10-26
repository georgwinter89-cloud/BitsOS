//! Key material that never shows (R8i): the PSK/PMK, the PTK's parts and the GTK live in
//! [`Secret`]s, which are wiped when dropped, print as `<secret>` in `Debug` and have no
//! `Display`. V1 wiped by hand (`crypto_wipe`, wlan/eapol.asm); spec/wlan.md §3 asks for
//! `zeroize` types instead.

use core::fmt;

use subtle::ConstantTimeEq;
use zeroize::Zeroize;

/// `N` bytes of key material, wiped on drop
pub struct Secret<const N: usize>([u8; N]);

impl<const N: usize> Secret<N> {
    pub const fn new(bytes: [u8; N]) -> Secret<N> {
        Secret(bytes)
    }
    /// Copies `src` (exactly `N` bytes), `None` otherwise
    pub fn from_slice(src: &[u8]) -> Option<Secret<N>> {
        let mut b = [0u8; N];
        if src.len() != N {
            return None;
        }
        b.copy_from_slice(src);
        Some(Secret(b))
    }
    /// The bytes, for the one place that hands them on (a cipher, a key slot)
    pub fn expose(&self) -> &[u8; N] {
        &self.0
    }
    pub fn expose_mut(&mut self) -> &mut [u8; N] {
        &mut self.0
    }
    /// A second copy, made on purpose (no `Clone`, so copies never happen by accident)
    pub fn duplicate(&self) -> Secret<N> {
        Secret(self.0)
    }
    /// Constant-time equality
    pub fn ct_eq(&self, other: &Secret<N>) -> bool {
        self.0.ct_eq(&other.0).into()
    }
}

impl<const N: usize> Drop for Secret<N> {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl<const N: usize> fmt::Debug for Secret<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<secret {N} bytes>")
    }
}

impl<const N: usize> PartialEq for Secret<N> {
    fn eq(&self, other: &Secret<N>) -> bool {
        self.ct_eq(other)
    }
}
impl<const N: usize> Eq for Secret<N> {}

/// A key of up to `N` bytes whose length is part of it (the GTK: 16 for CCMP, up to 32)
pub struct SecretVar<const N: usize> {
    bytes: Secret<N>,
    len: usize,
}

impl<const N: usize> SecretVar<N> {
    /// `None` when `src` is longer than `N`
    pub fn from_slice(src: &[u8]) -> Option<SecretVar<N>> {
        if src.len() > N {
            return None;
        }
        let mut b = Secret::new([0u8; N]);
        b.expose_mut()[..src.len()].copy_from_slice(src);
        Some(SecretVar {
            bytes: b,
            len: src.len(),
        })
    }
    pub fn expose(&self) -> &[u8] {
        &self.bytes.expose()[..self.len]
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn duplicate(&self) -> SecretVar<N> {
        SecretVar {
            bytes: self.bytes.duplicate(),
            len: self.len,
        }
    }
    /// Key and length both equal (hostap's check for a GTK in force), without an early exit
    pub fn ct_eq(&self, other: &SecretVar<N>) -> bool {
        let same_len = (self.len as u64).ct_eq(&(other.len as u64));
        (same_len & self.bytes.expose().ct_eq(other.bytes.expose())).into()
    }
}

impl<const N: usize> fmt::Debug for SecretVar<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<secret {} bytes>", self.len)
    }
}

/// A passphrase or hex PSK as CONFIG.TXT's `wlan_psk=` gave it, wiped on drop
pub struct Passphrase(alloc::vec::Vec<u8>);

impl Passphrase {
    pub fn new(bytes: &[u8]) -> Passphrase {
        Passphrase(bytes.to_vec())
    }
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Passphrase {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Passphrase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<passphrase>")
    }
}

/// Key material of a length known only at run time (unwrapped key data), wiped on drop
pub struct SecretBuf(alloc::vec::Vec<u8>);

impl SecretBuf {
    /// `len` zero bytes
    pub fn zeroed(len: usize) -> SecretBuf {
        SecretBuf(alloc::vec![0u8; len])
    }
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
    pub fn expose_mut(&mut self) -> &mut [u8] {
        &mut self.0
    }
}

impl Drop for SecretBuf {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for SecretBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<secret {} bytes>", self.0.len())
    }
}

/// Overwrites bytes that held key material (a copy on its way to the chip: an H2C buffer)
pub fn wipe(b: &mut [u8]) {
    b.zeroize();
}

/// Constant-time equality of two byte strings (a different length is unequal at once:
/// lengths are public)
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}
