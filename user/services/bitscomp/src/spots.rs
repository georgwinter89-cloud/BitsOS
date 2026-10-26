//! The mouse on bitscomp's own surfaces besides window frames (WY-5): the chrome (`chrome`), the start page
//! (`start`) and the address suggestions (`suggest`). A press on a spot acts when it is released over the same
//! spot, as a button does; a press anywhere else ends editing the address bar.

use crate::address::Where;
use crate::chrome::Part;
use crate::desk::Spot;
use crate::state::App;
use crate::wm::Action;

impl App {
    /// The address suggestion under (x, y); they lie over everything, floating windows too.
    pub fn suggest_at(&self, x: f64, y: f64) -> Option<usize> {
        if self.desk.edit.is_none() {
            return None;
        }
        let rows = self.suggestions();
        let (_, placed) = crate::suggest::layout(&rows, self.edit_field(), self.area_full().h - 8);
        placed.iter().position(|(_, r)| r.contains(x, y))
    }

    /// The spot under (x, y) besides the suggestions (`hit` asks those first, then floating windows): the chrome,
    /// then the start page.
    pub fn spot_at(&self, x: f64, y: f64) -> Option<Spot> {
        let ch = self.chrome();
        if let Some(p) = ch.hit(x, y) {
            return Some(Spot::Chrome(p));
        }
        if self.start_shown() && ch.work.contains(x, y) {
            let can = self.session.offered && self.session.saved.is_some();
            return self.start_page().hit(x, y, can).map(Spot::Start);
        }
        None
    }

    /// A press on a spot (any button but the middle one); true: bitscomp keeps it.
    pub fn spot_press(&mut self, s: Spot, right: bool) -> bool {
        let keeps_edit = matches!(s, Spot::Suggest(_) | Spot::Chrome(Part::Address));
        if !keeps_edit {
            self.address_end();
        }
        match s {
            // A page's right press opens the circle menu (no spot press then: the menu takes the release).
            Spot::Page(pid, part) if !self.page_press(pid, part, right) => return true,
            Spot::Page(..) => {}
            _ => self.page_edit_end(),
        }
        self.desk.press = Some(s);
        true
    }

    pub fn spot_release(&mut self) {
        let Some(s) = self.desk.press.take() else { return };
        // `hit`, not `spot_at`: a page's spots lie on windows (WY-5b).
        if matches!(self.hit(self.pointer), crate::wm::Hit::Spot(h) if h == s) {
            self.spot_act(s);
        }
    }

    pub fn spot_act(&mut self, s: Spot) {
        match s {
            Spot::Chrome(Part::Address) => {
                if self.desk.edit.as_ref().is_none_or(|e| e.tile.is_some()) {
                    self.address_begin(None);
                }
            }
            Spot::Chrome(Part::Ring) => self.act(Action::Sidebar),
            // WY-5c: the fold button folds a docked list and pins a revealed one; the top bar's button docks it.
            Spot::Chrome(Part::WsFold | Part::WsButton) => self.act(Action::Workspaces),
            // No tab has a history yet (the address layer comes later): the buttons are shown as not available.
            Spot::Chrome(Part::Back | Part::Forward) => {}
            Spot::Chrome(Part::Fav(i)) => {
                if let Some(a) = self.desk.favourites.get(i).cloned() {
                    self.open_address(&a, Where::Tab, None);
                }
            }
            Spot::Chrome(Part::Ws(i)) => self.act(Action::Workspace(i)),
            Spot::Chrome(Part::WsNew) => self.act(Action::NewWorkspace),
            Spot::Chrome(Part::Home) => {
                let has = !self.ws.tiles.is_empty() || !self.ws.floating.is_empty();
                self.desk.start_forced = has && !self.desk.start_forced;
                self.chrome_dirty();
            }
            Spot::Suggest(i) => {
                let rows = self.suggestions();
                let tile = self.desk.edit.as_ref().and_then(|e| e.tile);
                self.address_end();
                if let Some(r) = rows.get(i) {
                    self.open_address(&r.addr.clone(), Where::Tab, tile);
                }
            }
            Spot::Start(p) => self.start_act(p),
            Spot::Page(pid, part) => self.page_act(pid, part),
        }
    }

    fn start_act(&mut self, p: crate::start::Part) {
        use crate::start::Part as S;
        match p {
            S::Restore => self.session_restore(),
            S::New => self.session_new(),
            S::Fav(i) => {
                if let Some(a) = self.desk.favourites.get(i).cloned() {
                    self.open_address(&a, Where::Tile, None);
                }
            }
            S::Recent(i) => {
                if let Some(r) = self.session.recent.get(i).map(|r| r.addr.clone()) {
                    self.open_address(&r, Where::Tile, None);
                }
            }
        }
        self.desk.start_forced = false;
        self.chrome_dirty();
    }
}
