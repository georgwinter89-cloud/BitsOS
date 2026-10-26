//! Little-endian fields in on-disk buffers. Out-of-range reads give 0 and out-of-range writes are dropped:
//! a corrupt length never panics the kernel (the structure checks above reject such buffers first).

pub fn g8(b: &[u8], o: usize) -> u8 {
    b.get(o).copied().unwrap_or(0)
}
pub fn g16(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}
pub fn g32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
pub fn g64(b: &[u8], o: usize) -> u64 {
    g32(b, o) as u64 | (g32(b, o + 4) as u64) << 32
}
pub fn p8(b: &mut [u8], o: usize, v: u8) {
    if let Some(s) = b.get_mut(o) {
        *s = v;
    }
}
pub fn p16(b: &mut [u8], o: usize, v: u16) {
    if let Some(s) = b.get_mut(o..o + 2) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}
pub fn p32(b: &mut [u8], o: usize, v: u32) {
    if let Some(s) = b.get_mut(o..o + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}
pub fn p64(b: &mut [u8], o: usize, v: u64) {
    p32(b, o, v as u32);
    p32(b, o + 4, (v >> 32) as u32);
}
/// A value split into a low 32-bit and a high 16/32-bit field (block numbers, counts).
pub fn lohi(lo: u32, hi: u32) -> u64 {
    lo as u64 | (hi as u64) << 32
}
