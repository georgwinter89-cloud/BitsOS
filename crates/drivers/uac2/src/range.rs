//! The answer to a clock's RANGE request: a **2-byte** count, then that many 12-byte triplets of min, max and
//! resolution (F:566-567, F:359-361). Nothing here can be made to loop by a device: `contains` is arithmetic, not
//! enumeration.

const COUNT_BYTES: usize = 2;
const TRIPLET_BYTES: usize = 12;
/// wLength is 16 bits, so a table can only be asked for up to here.
pub const MAX_TRIPLETS: u16 = ((u16::MAX as usize - COUNT_BYTES) / TRIPLET_BYTES) as u16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeError {
    Truncated { need: usize, got: usize },
    TooManyTriplets(u16),
}

/// How many bytes to ask for once the first two have said how many triplets there are.
pub fn full_length(head: &[u8]) -> Result<u16, RangeError> {
    let count = count_of(head)?;
    if count > MAX_TRIPLETS {
        return Err(RangeError::TooManyTriplets(count));
    }
    Ok((COUNT_BYTES + count as usize * TRIPLET_BYTES) as u16)
}

fn count_of(reply: &[u8]) -> Result<u16, RangeError> {
    match reply {
        [lo, hi, ..] => Ok(u16::from_le_bytes([*lo, *hi])),
        _ => Err(RangeError::Truncated { need: COUNT_BYTES, got: reply.len() }),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Triplet {
    pub min: u32,
    pub max: u32,
    pub res: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ranges<'a> {
    table: &'a [u8],
}

impl<'a> Ranges<'a> {
    pub fn parse(reply: &'a [u8]) -> Result<Self, RangeError> {
        let count = count_of(reply)? as usize;
        let need = COUNT_BYTES + count * TRIPLET_BYTES;
        if reply.len() < need {
            return Err(RangeError::Truncated { need, got: reply.len() });
        }
        Ok(Ranges { table: &reply[COUNT_BYTES..need] })
    }

    /// The usable triplets. F:364-365 skips any that read negative as an int, or with max below min.
    pub fn triplets(&self) -> impl Iterator<Item = Triplet> + 'a {
        self.table
            .as_chunks::<TRIPLET_BYTES>()
            .0
            .iter()
            .map(|t| Triplet {
                min: u32::from_le_bytes([t[0], t[1], t[2], t[3]]),
                max: u32::from_le_bytes([t[4], t[5], t[6], t[7]]),
                res: u32::from_le_bytes([t[8], t[9], t[10], t[11]]),
            })
            .filter(|t| t.min <= i32::MAX as u32 && t.max <= i32::MAX as u32 && t.res <= i32::MAX as u32)
            .filter(|t| t.max >= t.min)
    }

    pub fn contains(&self, rate: u32) -> bool {
        self.triplets().any(|t| {
            if rate < t.min || rate > t.max {
                return false;
            }
            match t.res {
                // F:401: resolution 0 means the first value only.
                0 => rate == t.min,
                // F:372: a continuous range.
                1 => true,
                res => (rate - t.min).is_multiple_of(res),
            }
        })
    }
}
