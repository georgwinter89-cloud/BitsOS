//! Byte conversion of the user API types (little endian, x86_64 layout), so handlers work on the caller's bytes.

/// A user API type with a fixed C layout.
pub trait Wire: Sized + Copy {
    /// The all-zero value.
    const ZERO: Self;
    /// `sizeof` of the C type.
    const SIZE: usize = core::mem::size_of::<Self>();
    /// Reads the value from the first `SIZE` bytes; None if `b` is shorter.
    fn decode(b: &[u8]) -> Option<Self>;
    /// Writes the value into the first `SIZE` bytes, padding as zero; None if `out` is shorter.
    fn encode(&self, out: &mut [u8]) -> Option<()>;
}

macro_rules! int {
    ($($t:ty),*) => {$(
        impl Wire for $t {
            const ZERO: Self = 0;
            fn decode(b: &[u8]) -> Option<Self> {
                let a = b.get(..Self::SIZE)?.try_into().ok()?;
                Some(<$t>::from_le_bytes(a))
            }
            fn encode(&self, out: &mut [u8]) -> Option<()> {
                out.get_mut(..Self::SIZE)?.copy_from_slice(&self.to_le_bytes());
                Some(())
            }
        }
    )*};
}
int!(u8, i8, u16, i16, u32, i32, u64, i64);

impl<T: Wire, const N: usize> Wire for [T; N] {
    const ZERO: Self = [T::ZERO; N];

    fn decode(b: &[u8]) -> Option<Self> {
        let b = b.get(..Self::SIZE)?;
        let mut a = [T::ZERO; N];
        for (k, v) in a.iter_mut().enumerate() {
            *v = T::decode(b.get(k * T::SIZE..)?)?;
        }
        Some(a)
    }

    fn encode(&self, out: &mut [u8]) -> Option<()> {
        let o = out.get_mut(..Self::SIZE)?;
        for (k, v) in self.iter().enumerate() {
            v.encode(o.get_mut(k * T::SIZE..)?)?;
        }
        Some(())
    }
}
