//! SDMA 7.0 (the RX 9070 XT's SDMA 7.0.1): the packets where they differ from 6.0. The engine includes 6.0's
//! packet header (linux-7.3.1/drivers/gpu/drm/amd/amdgpu/sdma_v7_0.c:40), so every opcode, field and length of
//! [`crate::sdma`] holds, with two changes in what Linux emits:
//!
//! - COPY_LINEAR is eight dwords: the header carries CPV (bit 19, sdma_v6_0_0_pkt_open.h:131-135) and a DCC
//!   dword follows the destination (`sdma_v7_0_emit_copy_buffer` :1753-1785, `vm_copy_pte` :1058-1076; 0 unless
//!   the copy reads or writes compressed data);
//! - CONST_FILL's header carries COMPRESS (bit 16, sdma_v7_0.c:56-60; `sdma_v7_0_emit_fill_buffer` :1797-1808).
//!
//! The decoder here is strict about exactly these: a COPY_LINEAR without CPV is not a packet Linux 7.3.1 sends to
//! this engine, and its length (seven dwords as on 6.0, or eight) is not known, so it does not decode. Read with
//! 6.0's decoder, a 7.0 copy's last dword would pass as a NOP.

use alloc::vec::Vec;

use crate::encode::{EncodeError, push_addr};
use crate::sdma::{self, DecodeError, Packet, op};

/// `SDMA_PKT_COPY_LINEAR_HEADER_CPV(1)`.
pub const COPY_CPV: u32 = 1 << 19;
/// `SDMA_PKT_CONSTANT_FILL_HEADER_COMPRESS(1)`.
pub const FILL_COMPRESS: u32 = 1 << 16;
/// Dwords of a 7.0 COPY_LINEAR (`copy_num_dw` :1812).
pub const COPY_DW: usize = 8;

/// Length in dwords of the 7.0 packet at `words[0]`.
pub fn packet_len(words: &[u32]) -> Result<usize, DecodeError> {
    match words.first() {
        Some(&h) if sdma::hdr_op(h) == op::COPY && sdma::hdr_sub(h) == sdma::SUBOP_COPY_LINEAR => {
            if h & COPY_CPV != 0 { Ok(COPY_DW) } else { Err(DecodeError::Unknown { header: h }) }
        }
        _ => sdma::packet_len(words),
    }
}

/// Decodes the 7.0 packet at `words[0]`: the packet, its length, and for a COPY_LINEAR its DCC dword.
pub fn decode(words: &[u32]) -> Result<(Packet<'_>, usize, Option<u32>), DecodeError> {
    let n = packet_len(words)?;
    if words.len() < n {
        return Err(DecodeError::Truncated { need: n, have: words.len() });
    }
    if n == COPY_DW && sdma::hdr_op(words[0]) == op::COPY {
        // The first seven dwords are 6.0's copy.
        let (p, _) = sdma::decode(&words[..COPY_DW - 1])?;
        return Ok((p, n, Some(words[COPY_DW - 1])));
    }
    sdma::decode(words).map(|(p, n)| (p, n, None))
}

/// COPY_LINEAR of `bytes` (1..=2^30) bytes as Linux emits it for an uncompressed copy: CPV set, the DCC dword 0.
pub fn copy_linear(out: &mut Vec<u32>, src: u64, dst: u64, bytes: u64) -> Result<(), EncodeError> {
    if bytes == 0 || bytes > sdma::MAX_BYTES {
        return Err(EncodeError::Length { what: "COPY_LINEAR bytes", value: bytes });
    }
    out.push(sdma::header(op::COPY, sdma::SUBOP_COPY_LINEAR) | COPY_CPV);
    out.push(bytes as u32 - 1);
    out.push(0);
    push_addr(out, src);
    push_addr(out, dst);
    out.push(0);
    Ok(())
}

/// CONST_FILL of `bytes` bytes at `dst` with `data` as Linux emits it: COMPRESS set, the fill size field 0.
pub fn const_fill(out: &mut Vec<u32>, dst: u64, data: u32, bytes: u64) -> Result<(), EncodeError> {
    let at = out.len();
    sdma::const_fill(out, dst, data, bytes, sdma::FILL_BYTE)?;
    out[at] |= FILL_COMPRESS;
    Ok(())
}
