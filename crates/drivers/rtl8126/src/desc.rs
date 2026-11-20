//! Descriptors and ring rules, without memory. 16 bytes each (M:650-660): `opts1`, `opts2`, `addr`, little-endian.
//! The owner of DMA memory writes `addr` and `opts2` first and `opts1` last, with a write barrier between.

pub const DESC_BYTES: usize = 16;
pub const NUM_TX: u32 = 256; // M:74
pub const NUM_RX: u32 = 256; // M:75
pub const RING_BYTES: usize = NUM_TX as usize * DESC_BYTES;
pub const RING_ALIGN: usize = 256; // M:5090

/// Linux's sizes, kept on purpose (`plan/spec/gaming-pc.md` section 6): the chip writes as far as the descriptor says, and nothing
/// in BitsOS would catch an overrun. 2 KiB buffers on this chip are unverified.
pub const RX_BUF_SIZE: u32 = 16 * 1024 - 1; // M:73
pub const RX_MAX_SIZE: u16 = (RX_BUF_SIZE + 1) as u16; // M:2802
pub const RX_MEMORY: usize = NUM_RX as usize * (RX_BUF_SIZE as usize + 1);

pub const DESC_OWN: u32 = 1 << 31; // M:583
pub const RING_END: u32 = 1 << 30; // M:584
pub const FIRST_FRAG: u32 = 1 << 29; // M:585
pub const LAST_FRAG: u32 = 1 << 28; // M:586

const RX_RWT: u32 = 1 << 22; // M:472
const RX_RES: u32 = 1 << 21; // M:473
const RX_RUNT: u32 = 1 << 20; // M:474
const RX_CRC: u32 = 1 << 19; // M:475
const RX_LEN_MASK: u32 = 0x3fff; // M:4809
const FCS: u32 = 4;

const ETH_ZLEN: usize = 60; // M:4448
const TX_FRAME_MAX: usize = crate::seq::MTU as usize + 14 + 4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Desc {
    pub opts1: u32,
    pub opts2: u32,
    pub addr: u64,
}

impl Desc {
    pub fn to_bytes(self) -> [u8; DESC_BYTES] {
        let mut out = [0; DESC_BYTES];
        out[0..4].copy_from_slice(&self.opts1.to_le_bytes());
        out[4..8].copy_from_slice(&self.opts2.to_le_bytes());
        out[8..16].copy_from_slice(&self.addr.to_le_bytes());
        out
    }

    pub fn from_bytes(b: &[u8; DESC_BYTES]) -> Desc {
        let word = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Desc { opts1: word(0), opts2: word(4), addr: u64::from(word(8)) | (u64::from(word(12)) << 32) }
    }
}

const fn ring_end(entry: u32, count: u32) -> u32 {
    if entry == count - 1 { RING_END } else { 0 }
}

/// `rtl8169_alloc_rx_data` and the last line of `rtl8169_rx_fill` (M:4161-4210).
pub const fn rx_init(entry: u32, buf_addr: u64) -> Desc {
    Desc { opts1: DESC_OWN | ring_end(entry, NUM_RX) | RX_BUF_SIZE, opts2: 0, addr: buf_addr }
}

/// `rtl8169_mark_to_asic` (M:4151-4159): `addr` stays as it is (Linux reads it back at M:4830, so the chip does not
/// overwrite it), `opts2` is zeroed first, and only the ring-end bit survives from the old value.
pub const fn rx_rearm_opts1(old_opts1: u32) -> u32 {
    DESC_OWN | (old_opts1 & RING_END) | RX_BUF_SIZE
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rx {
    /// Still the chip's.
    Owned,
    /// A whole frame of this many bytes, without the checksum at its end.
    Frame(usize),
    Error { rwt: bool, runt: bool, crc: bool },
    /// M:4815-4816: "a symptom of over-mtu sized frames".
    Fragmented,
    /// Shorter than its own checksum. In Linux this would underflow into a failed allocation.
    TooShort,
}

/// `rtl_rx`'s look at one descriptor (M:4782-4821). Everything but `Owned` is given back with `rx_rearm_opts1`.
pub const fn rx_decode(opts1: u32) -> Rx {
    if opts1 & DESC_OWN != 0 {
        return Rx::Owned;
    }
    if opts1 & RX_RES != 0 {
        return Rx::Error { rwt: opts1 & RX_RWT != 0, runt: opts1 & RX_RUNT != 0, crc: opts1 & RX_CRC != 0 };
    }
    let len = opts1 & RX_LEN_MASK;
    if len < FCS {
        return Rx::TooShort;
    }
    if opts1 & (FIRST_FRAG | LAST_FRAG) != FIRST_FRAG | LAST_FRAG {
        return Rx::Fragmented;
    }
    Rx::Frame((len - FCS) as usize)
}

/// M:4448: this chip wants frames of at least 60 bytes when no offload is on. The filler must be zero (`skb_put_padto`).
pub const fn tx_padded_len(len: usize) -> usize {
    if len < ETH_ZLEN { ETH_ZLEN } else { len }
}

/// One frame in one descriptor (M:4328-4361, :4593-4607). Write `addr`, `opts2` and `opts1_prepare`, then a write
/// barrier, then `opts1_release`, then the doorbell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxPlan {
    pub addr: u64,
    pub len: usize,
    pub opts2: u32,
    pub opts1_prepare: u32,
    pub opts1_release: u32,
}

/// `None` for an empty frame or one longer than the MTU allows: no TSO here, so nothing longer can be right.
pub const fn tx_plan(entry: u32, addr: u64, frame_len: usize) -> Option<TxPlan> {
    if frame_len == 0 || frame_len > TX_FRAME_MAX {
        return None;
    }
    let len = tx_padded_len(frame_len);
    let opts1_prepare = len as u32 | ring_end(entry, NUM_TX) | LAST_FRAG;
    Some(TxPlan { addr, len, opts2: 0, opts1_prepare, opts1_release: opts1_prepare | DESC_OWN | FIRST_FRAG })
}

pub const fn tx_done(opts1: u32) -> bool {
    opts1 & DESC_OWN == 0
}

/// `cur_tx` and `dirty_tx` (M:4534, :4709): free-running, reduced to an entry only when used, so full and empty differ.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxCursor {
    cur: u32,
    dirty: u32,
}

impl TxCursor {
    #[cfg(test)]
    pub(crate) const fn at(cur: u32, dirty: u32) -> TxCursor {
        TxCursor { cur, dirty }
    }

    pub const fn slots_avail(&self) -> u32 {
        self.dirty.wrapping_add(NUM_TX).wrapping_sub(self.cur)
    }

    pub const fn in_flight(&self) -> u32 {
        self.cur.wrapping_sub(self.dirty)
    }

    /// The entry the next frame goes into, `None` when the ring is full.
    pub const fn next_entry(&self) -> Option<u32> {
        if self.slots_avail() == 0 { None } else { Some(self.cur % NUM_TX) }
    }

    pub const fn oldest_entry(&self) -> Option<u32> {
        if self.in_flight() == 0 { None } else { Some(self.dirty % NUM_TX) }
    }

    /// After `opts1_release` was written.
    pub fn sent(&mut self) {
        self.cur = self.cur.wrapping_add(1);
    }

    /// After one descriptor came back. True when the doorbell should ring once more: Linux does this on every chip
    /// because "TxPoll requests are lost when the Tx packets are too close" (M:4743-4752).
    pub fn reclaimed(&mut self) -> bool {
        self.dirty = self.dirty.wrapping_add(1);
        self.in_flight() != 0
    }
}
