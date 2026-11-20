//! CL-3b: where the conversation's parts lie (logical pixels), for the classic sidebar and the Bühne's column
//! alike: the list (newest at the bottom), the question card or the "not ready" card above the input, the input
//! line growing up to `INPUT_LINES`, "Neues Gespräch" and "Senden". Drawing (`assist_draw`), hit testing and
//! `ctl hot` read this one layout. Text is wrapped at the output's pixels, so it matches what is drawn; the
//! list's wrapped lines are kept (`Wraps`) and only what changed is wrapped again (an answer grows line by line).

use crate::assist::{Gate, Item, Model, ToolState, Who};
use crate::assist_level::Level;
use crate::chrome::logical_width;
use crate::layout::Rect;
use crate::text::{Font, Weight};
use crate::theme::Theme;

pub const PAD: i32 = 12;
/// A line of 14 px text, and of the small 12 px lines (tools, notes).
pub const LINE: i32 = 20;
pub const SMALL: i32 = 17;
pub const INPUT_LINES: usize = 6;
const BUTTON_H: i32 = 28;
/// Georg's lines are a bubble on the right, this much narrower than the list.
const INDENT: i32 = 36;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Body,
    Input,
    Send,
    New,
    Yes,
    No,
    Link,
    /// EINS-B (board 3): a chat of the list, its removal, the switch "Fenster vorne mitsehen", the level pill and
    /// one of its choices, "Papierkorb zeigen".
    Chat(usize),
    ChatRemove(usize),
    Sees,
    Level,
    LevelPick(Level),
    Trash,
    /// The model list's closed control, and a row of the open list.
    Model,
    ModelRow(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    Georg,
    Assistant,
    Tool(ToolState),
    Note { error: bool },
}

/// One wrapped line of the list at its place.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub x: i32,
    pub y: i32,
    pub text: String,
    pub style: Style,
}

#[derive(Clone, Debug, PartialEq)]
pub enum CardKind {
    /// A tool's question: Ja and Nein, which one the keyboard would press, whether Ja counts yet (it is shown
    /// pale until then), a yes that did not count.
    Question { yes: Rect, no: Rect, yes_picked: bool, armed: bool, note: Vec<String> },
    /// The assistant cannot answer: why, and the way to the settings when they can change it.
    Info { link: Option<Rect> },
}

#[derive(Clone, Debug, PartialEq)]
pub struct CardLook {
    pub rect: Rect,
    /// The line above the text ("Löschen fragt immer", "Schritt 2").
    pub title: Option<String>,
    pub lines: Vec<String>,
    /// Under the text: the question belongs to an ask that came through injected input.
    pub mark: Vec<String>,
    pub kind: CardKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct InputLook {
    pub rect: Rect,
    /// The lines shown (the cursor's among them).
    pub lines: Vec<String>,
    /// The cursor: line of `lines`, pixels from the line's start.
    pub caret: Option<(usize, i32)>,
    pub focus: bool,
    pub placeholder: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub body: Rect,
    pub list: Rect,
    pub rows: Vec<Row>,
    /// Georg's bubbles and the tools' marks, cut to the list.
    pub bubbles: Vec<Rect>,
    pub marks: Vec<(Rect, ToolState)>,
    /// What an empty conversation says.
    pub hint: Option<String>,
    pub card: Option<CardLook>,
    pub note: Option<(Rect, Vec<String>, bool)>,
    pub input: InputLook,
    pub new: Rect,
    pub send: Rect,
    pub send_label: &'static str,
    /// A question can be sent.
    pub live: bool,
    pub hover: Option<Part>,
    /// EINS-B: the chats' list (head and rows), the switch row, the footer's pill (or its three choices) and cloud
    /// line, the trash's notice.
    pub chats_head: Option<Rect>,
    pub chats: Vec<ChatLook>,
    pub sees: Option<SeesLook>,
    pub pill: Rect,
    pub pill_text: String,
    pub levels: Vec<(Level, Rect)>,
    pub cloud: Option<(Rect, String)>,
    pub trash: Option<(Rect, String)>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChatLook {
    pub rect: Rect,
    pub title: String,
    pub sub: String,
    pub remove: Rect,
    pub current: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SeesLook {
    pub switch: Rect,
    pub on: bool,
    pub now: String,
}

/// What the sidebar shows beside the conversation, from the App (settings, the window in front, the clock).
#[derive(Clone, Debug, Default)]
pub struct Side {
    /// "Fenster vorne mitsehen": on, and the window in front now.
    pub sees: Option<(bool, String)>,
    pub level: Option<Level>,
    pub cloud: Option<String>,
    pub now: i64,
    pub offset_min: i32,
}

/// The chats the list shows at once (the newest).
pub const CHAT_ROWS: usize = 4;
const CHAT_H: i32 = 40;
pub const SEES_TEXT: &str = "Fenster vorne mitsehen";
pub const NEW_CHAT: &str = "+ Neuer Chat";
pub const TRASH_LINK: &str = "Papierkorb zeigen";

pub const LINK_TEXT: &str = "KI-Einstellungen öffnen";
pub const NEW_TEXT: &str = NEW_CHAT;

pub fn text_font(t: &Theme) -> Font {
    crate::chrome_draw::font(t, 14.0, Weight::Regular)
}
pub fn small_font(t: &Theme) -> Font {
    crate::chrome_draw::font(t, 12.0, Weight::Regular)
}
pub fn button_font(t: &Theme) -> Font {
    crate::chrome_draw::font(t, 13.0, Weight::Medium)
}

/// `text` in lines of at most `max` pixels, as byte ranges: line ends stay, a line breaks after a space where it
/// can and inside a word that is longer than a line (an address, code).
pub fn wrap(f: &Font, text: &str, max: i32) -> Vec<(usize, usize)> {
    let boxes = f.boxes(text);
    let mut out = Vec::new();
    let (mut start, mut start_x, mut brk) = (0usize, 0i32, None::<(usize, usize)>);
    for (k, (i, c)) in text.char_indices().enumerate() {
        let (x, w) = boxes.get(k).copied().unwrap_or((0, 0));
        if c == '\n' {
            out.push((start, i));
            (start, start_x, brk) = (i + 1, x + w, None);
            continue;
        }
        if c != ' ' && x + w - start_x > max && i > start {
            // After the last space of this line, else right here.
            let (at, at_x) = match brk {
                Some((b, bk)) if b > start => (b, boxes.get(bk).map_or(x, |p| p.0)),
                _ => (i, x),
            };
            out.push((start, at));
            (start, start_x, brk) = (at, at_x, None);
        }
        if c == ' ' {
            brk = Some((i + 1, k + 1));
        }
    }
    out.push((start, text.len()));
    out
}

pub fn lines(f: &Font, text: &str, max: i32) -> Vec<String> {
    wrap(f, text, max).into_iter().map(|(a, b)| text[a..b].trim_end().to_string()).collect()
}

/// The list's wrapped items, kept between frames.
#[derive(Default)]
pub struct Wraps {
    key: (u64, i32),
    /// Per item: its lines, and the widest in pixels.
    items: Vec<(Vec<String>, i32)>,
}

fn px(l: i32, t: &Theme) -> i32 {
    (l as f64 * t.scale).round() as i32
}

fn item_text(it: &Item) -> String {
    match it {
        Item::Tool { name, text, state } => format!("{} ({})", if text.is_empty() { name } else { text }, state.word()),
        other => other.text().to_string(),
    }
}

fn style(it: &Item) -> Style {
    match it {
        Item::Turn { who: Who::Georg, .. } => Style::Georg,
        Item::Turn { who: Who::Assistant, .. } => Style::Assistant,
        Item::Tool { state, .. } => Style::Tool(*state),
        Item::Note { error, .. } => Style::Note { error: *error },
    }
}

/// (line height, room above and below the lines, gap to the next item) of a style.
fn metrics(s: Style) -> (i32, i32, i32) {
    match s {
        Style::Georg => (LINE, 6, 10),
        Style::Assistant => (LINE, 0, 10),
        Style::Tool(_) | Style::Note { .. } => (SMALL, 0, 8),
    }
}

/// Everything but the list's rows: cheap enough for hit testing.
pub fn parts(m: &Model, t: &Theme, body: Rect, hover: Option<Part>, side: &Side) -> Look {
    let (x, w) = (body.x + PAD, (body.w - 2 * PAD).max(1));
    let (tf, sf, bf) = (text_font(t), small_font(t), button_font(t));
    let mut y = body.y + body.h - PAD;
    let own = m.ask_id().is_some();
    let send_label = if own { "Abbrechen" } else { "Senden" };
    let send_w = logical_width(&bf, send_label, t.scale) + 24;
    let send = Rect { x: x + w - send_w, y: y - BUTTON_H, w: send_w, h: BUTTON_H };
    // The footer (board 3): the level pill, or its three choices, and the cloud line; Senden on the right.
    let level = side.level.unwrap_or(Level::Ask);
    let pill_text = format!("{}  ▾", level.name());
    let pill = Rect { x, y: y - BUTTON_H, w: logical_width(&bf, &pill_text, t.scale) + 28, h: BUTTON_H };
    let mut levels = Vec::new();
    if m.level_open {
        let mut lx = x;
        for l in Level::ALL {
            let lw = logical_width(&bf, l.name(), t.scale) + 20;
            levels.push((l, Rect { x: lx, y: y - BUTTON_H, w: lw, h: BUTTON_H }));
            lx += lw + 6;
        }
    }
    let after = if m.level_open { levels.last().map_or(pill.x + pill.w, |(_, r)| r.x + r.w) } else { pill.x + pill.w };
    let cloud = side.cloud.as_ref().filter(|_| after + 40 < send.x).map(|c| (Rect { x: after + 10, y: y - BUTTON_H, w: send.x - after - 18, h: BUTTON_H }, c.clone()));
    let new = Rect { x: x + w - (logical_width(&bf, NEW_CHAT, t.scale) + 20), y: body.y + 8, w: logical_width(&bf, NEW_CHAT, t.scale) + 20, h: BUTTON_H };
    y -= BUTTON_H + 8;
    // The input: the lines around the cursor.
    let inner = px(w - 20, t);
    let spans = wrap(&tf, &m.edit.text, inner);
    let at = m.edit.before().len();
    let cl = spans.iter().rposition(|(a, _)| *a <= at).unwrap_or(0);
    let shown = spans.len().min(INPUT_LINES);
    let first = if spans.len() <= INPUT_LINES { 0 } else { (cl + 1).saturating_sub(INPUT_LINES).min(spans.len() - INPUT_LINES) };
    let ih = shown as i32 * LINE + 16;
    let live = m.gate() == Gate::Ready;
    let input = InputLook {
        rect: Rect { x, y: y - ih, w, h: ih },
        lines: spans[first..first + shown].iter().map(|(a, b)| m.edit.text[*a..*b].to_string()).collect(),
        caret: m.focus.then(|| (cl - first, tf.width(&m.edit.text[spans[cl].0..at]))),
        focus: m.focus,
        placeholder: if live && m.edit.text.is_empty() { format!("Frag {} etwas", m.name) } else { String::new() },
    };
    y -= ih + 8;
    // "Fenster vorne mitsehen" with the window in front now (board 3).
    let sees = side.sees.as_ref().map(|(on, now)| {
        let h = LINE + SMALL;
        let r = Rect { x, y: y - h, w: 34, h: 20 };
        y -= h + 8;
        SeesLook { switch: r, on: *on, now: now.clone() }
    });
    let note = m.note.as_ref().map(|(text, error)| {
        let l = lines(&sf, text, px(w, t));
        let h = l.len() as i32 * SMALL;
        let r = Rect { x, y: y - h, w, h };
        y -= h + 6;
        (r, l, *error)
    });
    let card = card(m, t, x, w, &mut y, (&tf, &sf, &bf));
    let trash = m.trash.as_ref().map(|text| {
        let r = Rect { x, y: y - SMALL, w, h: SMALL };
        y -= SMALL + 6;
        (r, text.clone())
    });
    // The chats at the top: a head with "+ Neuer Chat", then the newest few.
    let mut top = body.y + 8 + BUTTON_H + 4;
    let chats_head = Some(Rect { x, y: body.y + 8, w: (new.x - x - 8).max(1), h: BUTTON_H });
    let chats: Vec<ChatLook> = m
        .chats
        .iter()
        .take(CHAT_ROWS)
        .map(|c| {
            let r = Rect { x, y: top, w, h: CHAT_H };
            top += CHAT_H + 2;
            let title = if c.title.is_empty() { "Ohne Titel".to_string() } else { c.title.clone() };
            ChatLook { rect: r, title, sub: c.sub(side.now, side.offset_min), remove: Rect { x: x + w - 26, y: r.y + 8, w: 24, h: 24 }, current: m.chat.as_deref() == Some(c.chat.as_str()) }
        })
        .collect();
    let list_top = top + 6;
    let hint = (m.items.is_empty() && card.is_none() && live).then(|| format!("Frag {} etwas.", m.name));
    let list = Rect { x, y: list_top, w, h: (y - list_top).max(0) };
    Look { body, list, rows: Vec::new(), bubbles: Vec::new(), marks: Vec::new(), hint, card, note, input, new, send, send_label, live, hover, chats_head, chats, sees, pill, pill_text, levels, cloud, trash }
}

fn card(m: &Model, t: &Theme, x: i32, w: i32, y: &mut i32, (tf, sf, bf): (&Font, &Font, &Font)) -> Option<CardLook> {
    let inner = px(w - 28, t);
    let (text, question) = match (&m.card, m.gate()) {
        (Some(c), _) => (c.text.clone(), true),
        (None, Gate::Stranger) => (m.stranger_line(), false),
        (None, Gate::Silent) => (m.silent_line(), false),
        (None, Gate::NotReady(why)) => (why, false),
        (None, _) => return None,
    };
    let l = lines(tf, &text, inner);
    let title = m.card.as_ref().and_then(|c| c.title.clone());
    let title_h = if title.is_some() { SMALL + 2 } else { 0 };
    let mark = match &m.card {
        Some(c) if c.remote => lines(sf, crate::assist::REMOTE_CARD, inner),
        _ => Vec::new(),
    };
    let text_h = title_h + l.len() as i32 * LINE + if mark.is_empty() { 0 } else { 4 + mark.len() as i32 * SMALL };
    let kind_h;
    let top = |h: i32, y: &i32| *y - (12 + text_h + h + 12);
    let kind = if question {
        let c = m.card.as_ref()?;
        let note = c.note.as_ref().map_or(Vec::new(), |n| lines(sf, n, inner));
        // The note stands above the buttons, so they stay where they are when it appears.
        let note_h = if note.is_empty() { 0 } else { 6 + note.len() as i32 * SMALL };
        kind_h = note_h + 10 + 30;
        let by = top(kind_h, y) + 12 + text_h + note_h + 10;
        let bw = |s: &str| logical_width(bf, s, t.scale) + 36;
        let yes = Rect { x: x + 14, y: by, w: bw("Ja"), h: 30 };
        let no = Rect { x: yes.x + yes.w + 10, y: by, w: bw("Nein"), h: 30 };
        CardKind::Question { yes, no, yes_picked: c.yes_picked, armed: c.armed, note }
    } else if matches!(m.gate(), Gate::NotReady(_)) {
        kind_h = 8 + 22;
        let ly = top(kind_h, y) + 12 + text_h + 8;
        CardKind::Info { link: Some(Rect { x: x + 14, y: ly, w: (logical_width(tf, LINK_TEXT, t.scale) + 2).min(w - 28), h: 22 }) }
    } else {
        kind_h = 0;
        CardKind::Info { link: None }
    };
    let rect = Rect { x, y: top(kind_h, y), w, h: 12 + text_h + kind_h + 12 };
    *y = rect.y - 10;
    Some(CardLook { rect, title, lines: l, mark, kind })
}

impl Look {
    pub fn hit(&self, x: f64, y: f64) -> Part {
        let on = |r: Rect| r.contains(x, y);
        if let Some(i) = self.chats.iter().position(|c| on(c.remove)) {
            return Part::ChatRemove(i);
        }
        if let Some(i) = self.chats.iter().position(|c| on(c.rect)) {
            return Part::Chat(i);
        }
        if let Some((l, _)) = self.levels.iter().find(|(_, r)| on(*r)) {
            return Part::LevelPick(*l);
        }
        if self.levels.is_empty() && on(self.pill) {
            return Part::Level;
        }
        if self.sees.as_ref().is_some_and(|s| on(Rect { w: s.switch.w + 200, ..s.switch })) {
            return Part::Sees;
        }
        if self.trash.as_ref().is_some_and(|(r, _)| on(*r)) {
            return Part::Trash;
        }
        match self.card.as_ref().map(|c| &c.kind) {
            Some(CardKind::Question { yes, .. }) if on(*yes) => return Part::Yes,
            Some(CardKind::Question { no, .. }) if on(*no) => return Part::No,
            Some(CardKind::Info { link: Some(l) }) if on(*l) => return Part::Link,
            _ => {}
        }
        if on(self.send) {
            Part::Send
        } else if on(self.new) {
            Part::New
        } else if on(self.input.rect) {
            Part::Input
        } else {
            Part::Body
        }
    }
}

/// Fills the list's visible rows; the scroll offset that is really there (the list's end is its limit).
pub fn rows(look: &mut Look, m: &mut Model, wraps: &mut Wraps, t: &Theme, generation: u64) -> i32 {
    let list = look.list;
    let key = (generation, px(list.w, t));
    if wraps.key != key {
        (wraps.key, wraps.items) = (key, Vec::new());
    }
    wraps.items.truncate(m.stale_from.min(m.items.len()));
    m.stale_from = usize::MAX;
    let (tf, sf) = (text_font(t), small_font(t));
    for it in &m.items[wraps.items.len()..] {
        let (f, w) = match style(it) {
            Style::Georg => (&tf, list.w - INDENT - 24),
            Style::Assistant => (&tf, list.w),
            Style::Tool(_) => (&sf, list.w - 10),
            Style::Note { .. } => (&sf, list.w),
        };
        let l = lines(f, &item_text(it), px(w.max(1), t));
        let wide = l.iter().map(|s| f.width(s)).max().unwrap_or(0);
        wraps.items.push((l, wide));
    }
    let height = |i: usize| {
        let (lh, pad, gap) = metrics(style(&m.items[i]));
        wraps.items[i].0.len() as i32 * lh + 2 * pad + gap
    };
    let total: i32 = (0..m.items.len()).map(height).sum();
    let most = (total - list.h).max(0);
    let scroll = m.scroll.clamp(0, most);
    let mut y = if total <= list.h { list.y } else { list.y + list.h - total + scroll };
    let (top, bottom) = (list.y, list.y + list.h);
    let cut = |r: Rect| {
        let (a, b) = (r.y.max(top), (r.y + r.h).min(bottom));
        (b > a).then_some(Rect { y: a, h: b - a, ..r })
    };
    for (i, it) in m.items.iter().enumerate() {
        let h = height(i);
        if y + h <= top || y >= bottom {
            y += h;
            continue;
        }
        let s = style(it);
        let (lh, pad, gap) = metrics(s);
        let (l, wide) = &wraps.items[i];
        let block = Rect { x: list.x, y, w: list.w, h: h - gap };
        let x = match s {
            Style::Georg => {
                let bw = ((*wide as f64 / t.scale).ceil() as i32 + 24).min(list.w);
                let b = Rect { x: list.x + list.w - bw, w: bw, ..block };
                look.bubbles.extend(cut(b));
                b.x + 12
            }
            Style::Tool(state) => {
                look.marks.extend(cut(Rect { w: 3, ..block }).map(|r| (r, state)));
                list.x + 10
            }
            _ => list.x,
        };
        for (k, text) in l.iter().enumerate() {
            let ly = y + pad + k as i32 * lh;
            if ly >= top && ly + lh <= bottom {
                look.rows.push(Row { x, y: ly, text: text.clone(), style: s });
            }
        }
        y += h;
    }
    scroll
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_keeps_line_ends_breaks_at_spaces_and_inside_long_words() {
        let f = Font::new(14.0, Weight::Regular, 1.0);
        let text = "eins zwei drei\nvier";
        let max = f.width("eins zwei") + 2;
        let got: Vec<&str> = wrap(&f, text, max).into_iter().map(|(a, b)| &text[a..b]).collect();
        assert_eq!(got, ["eins zwei ", "drei", "vier"]);
        let long = "https://example.org/ein/sehr/langer/pfad/ohne/leerzeichen";
        let parts = wrap(&f, long, f.width("https://example"));
        assert!(parts.len() > 2, "{parts:?}");
        assert!(parts.iter().all(|(a, b)| f.width(&long[*a..*b]) <= f.width("https://example") + 1), "a piece is wider than the line");
        assert_eq!(parts.iter().map(|(a, b)| &long[*a..*b]).collect::<String>(), long, "nothing is lost");
        assert_eq!(wrap(&f, "", 100), [(0, 0)]);
        assert_eq!(wrap(&f, "a\n", 100), [(0, 1), (2, 2)]);
    }
}
