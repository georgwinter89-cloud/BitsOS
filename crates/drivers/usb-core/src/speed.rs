//! Device speeds and what follows from them before any descriptor is read.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Speed {
    Low,
    Full,
    High,
    Super,
    SuperPlus,
}

impl Speed {
    /// xHCI's default Protocol Speed ID values (xHCI 1.2 §7.2.2.1.1), also used in the slot context.
    pub fn xhci_psiv(self) -> u8 {
        match self {
            Speed::Full => 1,
            Speed::Low => 2,
            Speed::High => 3,
            Speed::Super => 4,
            Speed::SuperPlus => 5,
        }
    }
    pub fn from_xhci_psiv(v: u8) -> Option<Speed> {
        Some(match v {
            1 => Speed::Full,
            2 => Speed::Low,
            3 => Speed::High,
            4 => Speed::Super,
            5 => Speed::SuperPlus,
            _ => return None,
        })
    }
    /// Endpoint 0's packet size until the device descriptor says (V1 `usb_mps_guess`): LS 8, FS/HS 64, SS 512.
    pub fn default_mps0(self) -> u16 {
        match self {
            Speed::Low => 8,
            Speed::Full | Speed::High => 64,
            Speed::Super | Speed::SuperPlus => 512,
        }
    }
    pub fn is_superspeed(self) -> bool {
        self >= Speed::Super
    }
    /// Low and full speed devices behind a high speed hub go through that hub's transaction translator.
    pub fn needs_tt(self) -> bool {
        self <= Speed::Full
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Speed::Low => "low",
            Speed::Full => "full",
            Speed::High => "high",
            Speed::Super => "super",
            Speed::SuperPlus => "super_plus",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn psiv_round_trip_and_guesses() {
        for s in [Speed::Low, Speed::Full, Speed::High, Speed::Super, Speed::SuperPlus] {
            assert_eq!(Speed::from_xhci_psiv(s.xhci_psiv()), Some(s));
        }
        assert_eq!(Speed::from_xhci_psiv(0), None);
        assert_eq!(Speed::from_xhci_psiv(15), None);
        assert_eq!((Speed::Low.default_mps0(), Speed::Full.default_mps0(), Speed::Super.default_mps0()), (8, 64, 512));
        assert!(Speed::Low.needs_tt() && Speed::Full.needs_tt() && !Speed::High.needs_tt());
    }
}
