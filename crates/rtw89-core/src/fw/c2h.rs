//! The chip's answers on RXQ: the C2H header and the receive/done acks (JS
//! js/fw-h2c.mjs `c2hParse`), and the minimal AX RX descriptor read t2-fw.mjs
//! uses to find a C2H in an RXQ buffer (the full RX parser is the txrx module's).

use alloc::vec::Vec;

use crate::bus::field;
use crate::error::{Rtw89Error, Rtw89Result};
use crate::tables::regs::*;

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let s = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Which ack a C2H carries
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckKind {
    /// RTW89_MAC_C2H_FUNC_REC_ACK: the firmware received the H2C
    Rec,
    /// RTW89_MAC_C2H_FUNC_DONE_ACK: the firmware handled it, with its return code
    Done,
}

/// The acked H2C, as rtw89_mac_c2h_rec_ack (mac.c:5458) / rtw89_mac_c2h_done_ack (mac.c:5494) read it
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct C2hAck {
    pub kind: AckKind,
    pub cat: u32,
    pub cls: u32,
    pub func: u32,
    pub seq: u32,
    /// H2C_RETURN of a done ack
    pub ret: Option<u32>,
}

/// struct rtw89_fw_c2h_attr plus the payload and the ack
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct C2h {
    pub cat: u32,
    pub cls: u32,
    pub func: u32,
    /// RTW89_C2H_HDR_W1_LEN: the C2H's length with its header
    pub len: u32,
    /// the acked H2C's sequence number (struct rtw89_c2h_hdr has none of its own)
    pub seq: Option<u32>,
    /// from after the 8-byte header up to `len` (cut at the buffer's end)
    pub payload: Vec<u8>,
    pub ack: Option<C2hAck>,
}

// Linux: rtw89_fw_c2h_parse_attr (fw.c:9043); for the receive and done acks also the acked
// H2C as rtw89_mac_c2h_rec_ack (mac.c:5458) / rtw89_mac_c2h_done_ack (mac.c:5494) read it.
// A buffer shorter than the header is refused (Linux trusts the chip here).
pub fn c2h_parse(buf: &[u8]) -> Rtw89Result<C2h> {
    let (Some(w0), Some(w1)) = (le32(buf, 0), le32(buf, 4)) else {
        return Err(Rtw89Error::Invalid {
            what: "C2H shorter than its header",
            value: buf.len() as u32,
        });
    };
    let cat = field(w0, RTW89_C2H_HDR_W0_CATEGORY);
    let cls = field(w0, RTW89_C2H_HDR_W0_CLASS);
    let func = field(w0, RTW89_C2H_HDR_W0_FUNC);
    let len = field(w1, RTW89_C2H_HDR_W1_LEN);
    let hl = RTW89_C2H_HEADER_LEN as usize;
    let end = (len as usize).min(buf.len()).max(hl);
    let payload = buf[hl..end].to_vec();
    let info = cat == RTW89_C2H_CAT_MAC && cls == RTW89_MAC_C2H_CLASS_INFO;
    let ack = match (info, func, le32(buf, 8)) {
        (true, f, Some(w2)) if f == RTW89_MAC_C2H_FUNC_REC_ACK => Some(C2hAck {
            kind: AckKind::Rec,
            cat: field(w2, RTW89_C2H_REV_ACK_W2_CAT),
            cls: field(w2, RTW89_C2H_REV_ACK_W2_CLASS),
            func: field(w2, RTW89_C2H_REV_ACK_W2_FUNC),
            seq: field(w2, RTW89_C2H_REV_ACK_W2_H2C_SEQ),
            ret: None,
        }),
        (true, f, Some(w2)) if f == RTW89_MAC_C2H_FUNC_DONE_ACK => Some(C2hAck {
            kind: AckKind::Done,
            cat: field(w2, RTW89_C2H_DONE_ACK_W2_CAT),
            cls: field(w2, RTW89_C2H_DONE_ACK_W2_CLASS),
            func: field(w2, RTW89_C2H_DONE_ACK_W2_FUNC),
            seq: field(w2, RTW89_C2H_DONE_ACK_W2_H2C_SEQ),
            ret: Some(field(w2, RTW89_C2H_DONE_ACK_W2_H2C_RETURN)),
        }),
        _ => None,
    };
    Ok(C2h {
        cat,
        cls,
        func,
        len,
        seq: ack.map(|a| a.seq),
        payload,
        ack,
    })
}

/// What an RXQ buffer holds, as far as finding a C2H needs
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxPkt {
    /// AX_RXD_RPKT_TYPE (RTW89_CORE_RX_TYPE_C2H = 10)
    pub typ: u32,
    /// AX_RXD_RPKT_LEN
    pub pkt_size: u32,
    /// the packet after the descriptor, cut at the buffer's end
    pub body: Vec<u8>,
}

// Linux: rtw89_core_query_rxdesc (core.c:4107), only what a C2H needs (t2-fw.mjs `rxDesc`):
// type, length, and where the packet starts (drv_info in 8-byte units, then the short or
// long descriptor; AX never fills `shift`). `data` is the buffer after its rxbd_info.
// `None` when the descriptor itself is not there.
pub fn rx_pkt(data: &[u8]) -> Option<RxPkt> {
    let w0 = le32(data, 0)?;
    let pkt_size = field(w0, AX_RXD_RPKT_LEN_MASK);
    let typ = field(w0, AX_RXD_RPKT_TYPE_MASK);
    let desc = if w0 & AX_RXD_LONG_RXD != 0 { 32 } else { 16 };
    let off = field(w0, AX_RXD_DRV_INFO_SIZE_MASK) as usize * 8 + desc;
    let start = off.min(data.len());
    let end = (off + pkt_size as usize).min(data.len());
    Some(RxPkt {
        typ,
        pkt_size,
        body: data[start..end].to_vec(),
    })
}
