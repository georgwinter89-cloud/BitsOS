//! EINS-D's questions (`ctl_bits_gate`) as the sidebar's Ja/Nein card (EINS-B's `assist_card`, whose rules make a
//! yes count only from Georg's own hand). One card at a time: a question waits while another card is up, and the
//! next one comes when that card goes. Without a sidebar on the screen it stays a notice and runs out.

use crate::assist::{Card, CardFor};
use crate::ctl_bits_gate::{self as gate, BY_ASSISTANT, BY_COMMAND};
use crate::input::Origin;
use crate::state::App;

impl App {
    /// The oldest open question onto the card when it is free; a card of a question that ran out goes.
    pub(crate) fn bits_card_show(&mut self) {
        if let Some(Card { of: CardFor::Bits(n), .. }) = self.assist.card {
            if gate::status(n) == "wartet" {
                return;
            }
            self.assist.card = None;
        }
        if self.assist.card.is_some() || self.assist_place().is_none() {
            return;
        }
        let Some(q) = gate::pending().into_iter().next() else { return };
        let undoable = q.line.is_some() || !q.text.contains("endgültig");
        self.assist.card = Some(Card {
            ask_id: q.n,
            of: CardFor::Bits(q.n),
            title: None,
            text: format!("{}{}", if q.from_tree { BY_ASSISTANT } else { BY_COMMAND }, q.text),
            // What cannot be undone starts on Nein (the sidebar's own remove question does the same).
            yes_picked: undoable,
            note: None,
            remote: false,
            since: crate::pacer::now(),
            armed: false,
        });
        self.chrome_dirty();
    }

    /// The card's answer (its rules already refused a yes that was not Georg's own).
    pub(crate) fn bits_card_decided(&mut self, n: u64, yes: bool) {
        if let Err(why) = gate::answer(self, n, yes, Origin::Physical) {
            self.assist.note = Some((why, true));
        }
    }
}
