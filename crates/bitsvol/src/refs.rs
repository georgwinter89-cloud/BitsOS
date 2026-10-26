//! Reference counts of data chunks: one byte per chunk, the rare count above 254 in a side map (snapshots share
//! whole subtrees, so a chunk's count is the number of leaf pages naming it, mostly 1).

use crate::error::{Result, VolError};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

const BIG: u8 = 0xFF;

pub struct RefCounts {
    small: Vec<u8>,
    big: BTreeMap<u64, u64>,
}

impl RefCounts {
    pub fn new(n: u64) -> Result<Self> {
        let n = usize::try_from(n).map_err(|_| VolError::NoMemory)?;
        let mut small = Vec::new();
        small.try_reserve_exact(n).map_err(|_| VolError::NoMemory)?;
        small.resize(n, 0);
        Ok(RefCounts { small, big: BTreeMap::new() })
    }

    pub fn get(&self, i: u64) -> u64 {
        match self.small.get(i as usize) {
            Some(&BIG) => self.big.get(&i).copied().unwrap_or(0),
            Some(&v) => v as u64,
            None => 0,
        }
    }

    pub fn inc(&mut self, i: u64) -> Result<()> {
        let s = self.small.get_mut(i as usize).ok_or(VolError::Corrupt("chunk out of range"))?;
        match *s {
            BIG => *self.big.get_mut(&i).ok_or(VolError::Corrupt("refcount map"))? += 1,
            v if v + 1 == BIG => {
                *s = BIG;
                self.big.insert(i, BIG as u64);
            }
            v => *s = v + 1,
        }
        Ok(())
    }

    /// Returns the new count; dropping a reference that does not exist is corruption, never a wrap.
    pub fn dec(&mut self, i: u64) -> Result<u64> {
        let s = self.small.get_mut(i as usize).ok_or(VolError::Corrupt("chunk out of range"))?;
        match *s {
            0 => Err(VolError::Corrupt("refcount below zero")),
            BIG => {
                let b = self.big.get_mut(&i).ok_or(VolError::Corrupt("refcount map"))?;
                *b -= 1;
                let n = *b;
                if n < BIG as u64 {
                    self.big.remove(&i);
                    *s = n as u8;
                }
                Ok(n)
            }
            v => {
                *s = v - 1;
                Ok((v - 1) as u64)
            }
        }
    }

    /// Index of the first zero count in `from..len`.
    pub fn next_zero(&self, from: u64) -> Option<u64> {
        let f = from as usize;
        self.small.get(f..)?.iter().position(|&v| v == 0).map(|p| (f + p) as u64)
    }
}
