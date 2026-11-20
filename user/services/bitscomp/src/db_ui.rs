//! DB3, EINS-B: the popup in DB1's list (window `desktop`), so the desk and an agent with Georg's release see it as
//! text: what it sees, the input line, the text, the buttons. Ja is a question's answer (`settles`): MCP never
//! presses it, and any injected press of it is refused by the popup. The assistant's own `handeln` never acts in
//! it (`db_act_own`).

use uilist::{Action, Element, Role, State};

use crate::state::App;

/// The popup's group name in the list.
pub const GROUP: &str = "Durchblick";

impl App {
    pub(crate) fn ui_durchblick(&self) -> Option<Element> {
        let g = self.db_geo()?;
        let px = |r| self.ui_px(r);
        let mut grp = Element::new(Role::Group, GROUP, px(g.panel)).key("durchblick");
        grp = grp.child(Element::new(Role::Text, "Sieht", px(g.head)).key("durchblick:sees").value(self.db_sees()));
        let field = if self.db.has_field() {
            Element::new(Role::Field, "Frage an den Assistenten", px(g.field)).key("durchblick:field").value(self.db.field.text.clone()).acts(&[Action::Type, Action::Focus]).state_if(State::Focused, self.db.focus.is_none())
        } else {
            Element::new(Role::Text, "Frage", px(g.field)).key("durchblick:question").value(self.db.question.clone())
        };
        grp = grp.child(field);
        grp = grp.child(Element::new(Role::Text, "Antwort", px(g.body)).key("durchblick:text").value(self.db_texts()));
        for (b, r) in &g.buttons {
            let e = Element::new(Role::Button, b.label(), px(*r)).key(format!("durchblick:{}", b.word())).acts(&[Action::Press]).state_if(State::Focused, self.db.focus == Some(*b));
            grp = grp.child(if b.consents() { e.settles() } else { e });
        }
        Some(grp)
    }
}
