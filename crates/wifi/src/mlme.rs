//! Joining one AP, chip independent: open authentication, association with our RSN element, the
//! 4-way handshake, then the link until the AP or we end it. A state machine without I/O: the
//! driver sends what an [`Action`] asks for, feeds what came in ([`Mlme::frame`],
//! [`Mlme::eapol`]) and tells it after every RX poll ([`Mlme::poll_done`]), which runs the timers.
//!
//! The order and the timing are sta-link.mjs's (proven on the Mini-PC, join + DHCP), which is
//! mac80211 mlme.c's: IEEE80211_AUTH_TIMEOUT (linux-6.6/net/mac80211/mlme.c:34) and
//! IEEE80211_ASSOC_TIMEOUT (linux-6.6/net/mac80211/mlme.c:41) 200 ms (HZ / 5),
//! IEEE80211_AUTH_MAX_TRIES (linux-6.6/net/mac80211/mlme.c:38) 3, and
//! an exchange waits at least three RX polls; the handshake gets 8 s, ends with reason 15 when
//! no PTK came (4-way handshake timeout). The keys go in in wpa_supplicant's order (hostap wpa.c):
//! 3/4 -> 4/4 out, then the PTK, then the GTK; group 1/2 -> the GTK, then 2/2 out. Not in the JS's
//! software path but in V1's in-kernel join (join-kit / WlJoin): a deauthentication or
//! disassociation by the AP during authentication or association ends the join at once.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::eapol::{Accepted, GtkInstall, Random, Supplicant};
use crate::events::{EventLog, WifiEvent};
use crate::scan::Mac;
use crate::secret::Secret;

/// IEEE 802.11 reason codes BitsOS sends
pub mod reason {
    /// leaving (disconnect, interface down)
    pub const DEAUTH_LEAVING: u16 = 3;
    /// 4-way handshake timeout
    pub const FOURWAY_TIMEOUT: u16 = 15;
}

/// The timing of a join (mac80211's numbers by default)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinTiming {
    /// tries per exchange (IEEE80211_AUTH_MAX_TRIES / _ASSOC_MAX_TRIES)
    pub tries: u32,
    /// ms one try waits for its answer (IEEE80211_AUTH_TIMEOUT, HZ / 5)
    pub exchange_ms: u64,
    /// RX polls one try waits at least (sta-link.mjs: an MCP round trip is slow)
    pub min_polls: u32,
    /// ms the 4-way handshake may take (sta-link.mjs connect's timeoutMs)
    pub handshake_ms: u64,
}

impl Default for JoinTiming {
    fn default() -> JoinTiming {
        JoinTiming {
            tries: 3,
            exchange_ms: 200,
            min_polls: 3,
            handshake_ms: 8000,
        }
    }
}

/// Where a join or link is
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// authentication sent, waiting for the answer
    Auth,
    /// association request sent
    Assoc,
    /// associated, keys not in yet
    Handshake,
    /// the port is authorized
    Connected,
    /// the join failed or the link ended
    Down,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Auth => "authentication",
            Phase::Assoc => "association",
            Phase::Handshake => "4-way handshake",
            Phase::Connected => "connected",
            Phase::Down => "down",
        }
    }
}

/// A management frame for the station, parsed by the driver
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mgmt {
    Auth {
        a1: Mac,
        a2: Mac,
        alg: u16,
        seq: u16,
        status: u16,
    },
    AssocResp {
        a1: Mac,
        a2: Mac,
        status: u16,
        aid: u16,
        capability: u16,
    },
    Deauth {
        a1: Mac,
        a2: Mac,
        reason: u16,
    },
    Disassoc {
        a1: Mac,
        a2: Mac,
        reason: u16,
    },
}

/// Why a join failed
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinFailure {
    AuthTimeout,
    AuthRefused {
        status: u16,
    },
    AssocTimeout,
    AssocRefused {
        status: u16,
    },
    /// the AP ended it during authentication, association or the handshake
    EndedByAp {
        phase: Phase,
        reason: u16,
    },
    HandshakeTimeout,
}

impl fmt::Display for JoinFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinFailure::AuthTimeout => f.write_str("authentication timed out"),
            JoinFailure::AuthRefused { status } => {
                write!(f, "authentication refused, status {status}")
            }
            JoinFailure::AssocTimeout => f.write_str("association timed out"),
            JoinFailure::AssocRefused { status } => {
                write!(f, "association refused, status {status}")
            }
            JoinFailure::EndedByAp {
                phase: Phase::Handshake,
                reason,
            } => write!(
                f,
                "deauthenticated by the AP during the handshake, reason {reason}"
            ),
            JoinFailure::EndedByAp { phase, reason } => {
                write!(
                    f,
                    "deauthenticated by the AP during {}, reason {reason}",
                    phase.as_str()
                )
            }
            JoinFailure::HandshakeTimeout => f.write_str("4-way handshake timed out"),
        }
    }
}

/// What the driver does next, in this order
#[derive(Debug)]
pub enum Action {
    /// send the open authentication (seq 1); `attempt` from 1
    SendAuth { attempt: u32 },
    /// send the association request with our RSN element
    SendAssoc { attempt: u32 },
    /// the association went through: ieee80211_assoc_success's driver steps (EDCA, ASSOC,
    /// the ERP slot from the capability)
    Associated { aid: u16, capability: u16 },
    /// an EAPOL frame (from its version byte) to the AP
    SendEapol(Vec<u8>),
    /// the pairwise key (key ID 0) into the chip; a key there is replaced
    InstallPtk(Secret<16>),
    /// a group key into the chip; a key of the same ID there is replaced
    InstallGtk(GtkInstall),
    /// the port is authorized: PTK and GTK are in (`group_key` is always true since WL-3's fix;
    /// kept so the netdev can assert it before DHCP starts)
    Connected { aid: u16, group_key: bool },
    /// the join failed: tear down; `deauth` is the reason of the deauthentication to send
    /// (`None`: no frame of ours, the AP left or never answered)
    Failed {
        why: JoinFailure,
        deauth: Option<u16>,
    },
    /// the AP ended the link after the join (deauthentication or disassociation): tear down
    /// without a frame of ours
    Ended { reason: u16, disassoc: bool },
}

/// One join and the link after it
pub struct Mlme {
    own: Mac,
    bssid: Mac,
    phase: Phase,
    timing: JoinTiming,
    attempt: u32,
    polls: u32,
    deadline: u64,
    aid: u16,
    ptk_in: bool,
    gtk_in: bool,
    sup: Supplicant,
    events: EventLog<WifiEvent>,
}

impl fmt::Debug for Mlme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Mlme")
            .field("bssid", &self.bssid)
            .field("phase", &self.phase)
            .field("attempt", &self.attempt)
            .field("aid", &self.aid)
            .field("ptk_in", &self.ptk_in)
            .field("gtk_in", &self.gtk_in)
            .finish()
    }
}

impl Mlme {
    // Linux: none (mac80211 ieee80211_mgd_auth -> ieee80211_auth): the first authentication;
    // `sup` is the link's handshake context (sta-link.mjs's eapol callback), closed when the
    // link ends
    pub fn start(
        own: Mac,
        bssid: Mac,
        sup: Supplicant,
        timing: JoinTiming,
        now: u64,
    ) -> (Mlme, Vec<Action>) {
        let mut m = Mlme {
            own,
            bssid,
            phase: Phase::Auth,
            timing,
            attempt: 1,
            polls: 0,
            deadline: now.saturating_add(timing.exchange_ms),
            aid: 0,
            ptk_in: false,
            gtk_in: false,
            sup,
            events: EventLog::new(),
        };
        m.events.push(WifiEvent::AuthSent { attempt: 1 });
        (m, vec![Action::SendAuth { attempt: 1 }])
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn bssid(&self) -> Mac {
        self.bssid
    }
    pub fn aid(&self) -> u16 {
        self.aid
    }
    /// The port is authorized (mac80211's AUTHORIZED)
    pub fn authorized(&self) -> bool {
        self.phase == Phase::Connected
    }
    /// The pairwise key is in
    pub fn ptk_in(&self) -> bool {
        self.ptk_in
    }
    pub fn take_events(&mut self) -> Vec<WifiEvent> {
        self.events.take()
    }
    pub fn events(&self) -> &[WifiEvent] {
        self.events.as_slice()
    }

    /// The link ends (our disconnect, a teardown): the handshake context is wiped
    pub fn close(&mut self) {
        self.phase = Phase::Down;
        self.sup.close();
    }

    fn fail(&mut self, why: JoinFailure, deauth: Option<u16>) -> Vec<Action> {
        self.close();
        self.events.push(WifiEvent::JoinFailed { why });
        vec![Action::Failed { why, deauth }]
    }

    fn next_try(&mut self, now: u64) -> Vec<Action> {
        self.polls = 0;
        self.deadline = now.saturating_add(self.timing.exchange_ms);
        self.attempt += 1;
        let attempt = self.attempt;
        if self.phase == Phase::Auth {
            self.events.push(WifiEvent::AuthSent { attempt });
            vec![Action::SendAuth { attempt }]
        } else {
            self.events.push(WifiEvent::AssocSent { attempt });
            vec![Action::SendAssoc { attempt }]
        }
    }

    // Linux: none (mac80211 ieee80211_rx_mgmt_auth (linux-6.6/net/mac80211/mlme.c:3652),
    // ieee80211_rx_mgmt_assoc_resp (linux-6.6/net/mac80211/mlme.c:5279), ieee80211_rx_mgmt_deauth
    // (linux-6.6/net/mac80211/mlme.c:3817), ieee80211_rx_mgmt_disassoc
    // (linux-6.6/net/mac80211/mlme.c:3861) for a station): one management frame from the air.
    // Frames not for this join are ignored.
    pub fn frame(&mut self, m: &Mgmt, now: u64) -> Vec<Action> {
        match (*m, self.phase) {
            (
                Mgmt::Auth {
                    a1,
                    a2,
                    seq,
                    status,
                    ..
                },
                Phase::Auth,
            ) if a2 == self.bssid && a1 == self.own && seq == 2 => {
                if status != 0 {
                    return self.fail(JoinFailure::AuthRefused { status }, None);
                }
                // NONE -> AUTH, then the association request
                self.events.push(WifiEvent::AuthOk);
                self.phase = Phase::Assoc;
                self.attempt = 0;
                self.next_try(now)
            }
            (
                Mgmt::AssocResp {
                    a1,
                    a2,
                    status,
                    aid,
                    capability,
                },
                Phase::Assoc,
            ) if a2 == self.bssid && a1 == self.own => {
                if status != 0 {
                    return self.fail(JoinFailure::AssocRefused { status }, None);
                }
                // ieee80211_assoc_success: AUTH -> ASSOC; the AID's two top bits are flags
                self.aid = aid & 0x3fff;
                self.phase = Phase::Handshake;
                self.polls = 0;
                self.deadline = now.saturating_add(self.timing.handshake_ms);
                self.events.push(WifiEvent::Associated { aid: self.aid });
                vec![Action::Associated {
                    aid: self.aid,
                    capability,
                }]
            }
            (Mgmt::Deauth { a1, a2, reason }, p) | (Mgmt::Disassoc { a1, a2, reason }, p)
                if a2 == self.bssid && (a1 == self.own || a1 == [0xff; 6]) && p != Phase::Down =>
            {
                let disassoc = matches!(m, Mgmt::Disassoc { .. });
                if p == Phase::Connected {
                    self.close();
                    self.events.push(WifiEvent::EndedByAp { reason, disassoc });
                    return vec![Action::Ended { reason, disassoc }];
                }
                self.fail(JoinFailure::EndedByAp { phase: p, reason }, None)
            }
            _ => Vec::new(),
        }
    }

    // Linux: none (sta-link.mjs eapolFrame; hostap wpa_sm_rx_eapol and the order of
    // wpa_supplicant_process_3_of_4 / _1_of_2): an EAPOL frame of our AP to us (from its version
    // byte), from association on
    pub fn eapol(&mut self, pdu: &[u8], rng: &mut dyn Random) -> Vec<Action> {
        if !matches!(self.phase, Phase::Handshake | Phase::Connected) {
            return Vec::new();
        }
        let Accepted {
            got,
            answer,
            ptk,
            gtk,
        } = match self.sup.receive(pdu, rng) {
            Ok(a) => a,
            Err(why) => {
                self.events.push(WifiEvent::EapolDropped { why });
                return Vec::new();
            }
        };
        self.events.push(WifiEvent::EapolTaken { got });
        let mut out = Vec::new();
        let group_only = gtk.is_some() && ptk.is_none() && got == crate::eapol::Got::Group;
        if group_only {
            // group 1/2: the new GTK, then 2/2 out
            if let Some(g) = gtk {
                self.events.push(WifiEvent::GtkInstalled { id: g.id });
                self.gtk_in = true;
                out.push(Action::InstallGtk(g));
            }
            out.push(Action::SendEapol(answer));
            return out;
        }
        // 3/4: 4/4 out, then the PTK, then the GTK (1/4: 2/4 out only)
        out.push(Action::SendEapol(answer));
        if let Some(tk) = ptk {
            self.ptk_in = true;
            self.events.push(WifiEvent::PtkInstalled);
            out.push(Action::InstallPtk(tk));
        } else if got == crate::eapol::Got::Msg3 {
            self.events.push(WifiEvent::PtkKept);
        }
        match gtk {
            Some(g) => {
                self.gtk_in = true;
                self.events.push(WifiEvent::GtkInstalled { id: g.id });
                out.push(Action::InstallGtk(g));
            }
            None if got != crate::eapol::Got::Msg1 => {
                if let Some(id) = self.sup.gtk_id() {
                    self.events.push(WifiEvent::GtkKept { id });
                }
            }
            None => {}
        }
        out
    }

    // Linux: none (mac80211's auth/assoc timers, sta-link.mjs mlmeExchange's and connect's wait
    // loops): after every RX poll of a join. An exchange waits at least `min_polls` polls and
    // until its deadline, then tries again or gives up; the handshake ends with both keys in, or
    // fails at its deadline.
    pub fn poll_done(&mut self, now: u64) -> Vec<Action> {
        self.polls = self.polls.saturating_add(1);
        match self.phase {
            Phase::Auth | Phase::Assoc => {
                if self.polls < self.timing.min_polls || now < self.deadline {
                    return Vec::new();
                }
                if self.attempt < self.timing.tries {
                    return self.next_try(now);
                }
                let why = if self.phase == Phase::Auth {
                    JoinFailure::AuthTimeout
                } else {
                    JoinFailure::AssocTimeout
                };
                self.fail(why, None)
            }
            Phase::Handshake => {
                if self.ptk_in && self.gtk_in {
                    return self.connected();
                }
                if now < self.deadline {
                    return Vec::new();
                }
                // Changed from the JS (a PTK alone counted at the deadline): without the GTK no
                // broadcast can be read, so DHCP's answers would be lost (spec/wlan.md §2.4
                // trap 4); the port opens only with both keys (wpa_supplicant authorizes after
                // the GTK of message 3 too). Our supplicant refuses a message 3 without a GTK,
                // so this is a guard, not a path the AP can take.
                self.fail(JoinFailure::HandshakeTimeout, Some(reason::FOURWAY_TIMEOUT))
            }
            Phase::Connected | Phase::Down => Vec::new(),
        }
    }

    fn connected(&mut self) -> Vec<Action> {
        self.phase = Phase::Connected;
        self.events.push(WifiEvent::Connected {
            aid: self.aid,
            group_key: self.gtk_in,
        });
        vec![Action::Connected {
            aid: self.aid,
            group_key: self.gtk_in,
        }]
    }
}
