//! WY-3: the few X11 core and extension requests the window manager sends, and the fields of what comes back.
//! Little-endian wire format (the setup asks for it), lengths in 4-byte units (X Window System Protocol, X11R7).
//! No x11rb: it is not among the crates V2 may download (plan/spec/rust-ecosystem.md), so the WM speaks the
//! protocol itself; only what bitscomp needs is here.

/// A request body under construction; `done` pads it and writes the length.
pub struct Req(Vec<u8>);

impl Req {
    pub fn new(op: u8, data: u8) -> Self {
        Req(vec![op, data, 0, 0])
    }
    pub fn u8(mut self, v: u8) -> Self {
        self.0.push(v);
        self
    }
    pub fn u16(mut self, v: u16) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i16(self, v: i16) -> Self {
        self.u16(v as u16)
    }
    pub fn u32(mut self, v: u32) -> Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn bytes(mut self, b: &[u8]) -> Self {
        self.0.extend_from_slice(b);
        self
    }
    pub fn pad(mut self) -> Self {
        while !self.0.len().is_multiple_of(4) {
            self.0.push(0);
        }
        self
    }
    pub fn done(self) -> Vec<u8> {
        let mut v = self.pad().0;
        let units = (v.len() / 4) as u16;
        v[2..4].copy_from_slice(&units.to_le_bytes());
        v
    }
}

/// The connection setup: little-endian, protocol 11.0, no authorization (the -wm socket is the server's own).
pub fn setup_request() -> Vec<u8> {
    vec![b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0]
}

pub fn u16_at(b: &[u8], i: usize) -> u16 {
    b.get(i..i + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}
pub fn i16_at(b: &[u8], i: usize) -> i16 {
    u16_at(b, i) as i16
}
pub fn u32_at(b: &[u8], i: usize) -> u32 {
    b.get(i..i + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// What the WM keeps of the setup reply.
#[derive(Debug, Clone, Default)]
pub struct Setup {
    pub id_base: u32,
    pub id_mask: u32,
    pub root: u32,
    pub width: u16,
    pub height: u16,
}

/// The setup reply (8 bytes head + `length` units); None when the server refused or the reply is short.
pub fn parse_setup(b: &[u8]) -> Result<Setup, String> {
    if b.first() != Some(&1) {
        let n = b.get(1).copied().unwrap_or(0) as usize;
        let why = b.get(8..8 + n).map(|s| String::from_utf8_lossy(s).into_owned()).unwrap_or_default();
        return Err(format!("X setup refused: {why}"));
    }
    let vendor_len = u16_at(b, 24) as usize;
    let formats = b.get(29).copied().unwrap_or(0) as usize;
    let screen = 40 + vendor_len.div_ceil(4) * 4 + formats * 8;
    if b.len() < screen + 40 || b.get(28).copied().unwrap_or(0) == 0 {
        return Err("X setup reply without a screen".into());
    }
    Ok(Setup {
        id_base: u32_at(b, 12),
        id_mask: u32_at(b, 16),
        root: u32_at(b, screen),
        width: u16_at(b, screen + 20),
        height: u16_at(b, screen + 22),
    })
}

// Core opcodes.
pub const CREATE_WINDOW: u8 = 1;
pub const CHANGE_WINDOW_ATTRIBUTES: u8 = 2;
pub const MAP_WINDOW: u8 = 8;
pub const CONFIGURE_WINDOW: u8 = 12;
pub const INTERN_ATOM: u8 = 16;
pub const GET_ATOM_NAME: u8 = 17;
pub const CHANGE_PROPERTY: u8 = 18;
pub const GET_PROPERTY: u8 = 20;
pub const SET_SELECTION_OWNER: u8 = 22;
pub const CONVERT_SELECTION: u8 = 24;
pub const SEND_EVENT: u8 = 25;
pub const SET_INPUT_FOCUS: u8 = 42;
pub const QUERY_EXTENSION: u8 = 98;
pub const KILL_CLIENT: u8 = 113;

// Events.
pub const CREATE_NOTIFY: u8 = 16;
pub const DESTROY_NOTIFY: u8 = 17;
pub const UNMAP_NOTIFY: u8 = 18;
pub const MAP_NOTIFY: u8 = 19;
pub const MAP_REQUEST: u8 = 20;
pub const CONFIGURE_NOTIFY: u8 = 22;
pub const CONFIGURE_REQUEST: u8 = 23;
pub const PROPERTY_NOTIFY: u8 = 28;
pub const SELECTION_CLEAR: u8 = 29;
pub const SELECTION_REQUEST: u8 = 30;
pub const SELECTION_NOTIFY: u8 = 31;
pub const CLIENT_MESSAGE: u8 = 33;

// Event masks.
pub const STRUCTURE_NOTIFY: u32 = 1 << 17;
pub const SUBSTRUCTURE_NOTIFY: u32 = 1 << 19;
pub const SUBSTRUCTURE_REDIRECT: u32 = 1 << 20;
pub const FOCUS_CHANGE: u32 = 1 << 21;
pub const PROPERTY_CHANGE: u32 = 1 << 22;

/// Window attribute bits (CreateWindow, ChangeWindowAttributes).
pub const CW_OVERRIDE_REDIRECT: u32 = 1 << 9;
pub const CW_EVENT_MASK: u32 = 1 << 11;

pub const ATOM_ATOM: u32 = 4;
pub const ATOM_CARDINAL: u32 = 6;
pub const ATOM_STRING: u32 = 31;
pub const ATOM_WINDOW: u32 = 33;
pub const ATOM_WM_HINTS: u32 = 35;
pub const ATOM_WM_NAME: u32 = 39;
pub const ATOM_WM_CLASS: u32 = 67;
pub const ATOM_WM_TRANSIENT_FOR: u32 = 68;

#[allow(clippy::too_many_arguments)]
pub fn create_window(wid: u32, parent: u32, x: i16, y: i16, w: u16, h: u16, class: u16, mask: u32, values: &[u32]) -> Vec<u8> {
    let mut r = Req::new(CREATE_WINDOW, 0).u32(wid).u32(parent).i16(x).i16(y).u16(w).u16(h).u16(0).u16(class).u32(0).u32(mask);
    for v in values {
        r = r.u32(*v);
    }
    r.done()
}

pub fn change_window_attributes(win: u32, mask: u32, values: &[u32]) -> Vec<u8> {
    let mut r = Req::new(CHANGE_WINDOW_ATTRIBUTES, 0).u32(win).u32(mask);
    for v in values {
        r = r.u32(*v);
    }
    r.done()
}

pub fn map_window(win: u32) -> Vec<u8> {
    Req::new(MAP_WINDOW, 0).u32(win).done()
}

pub const CONFIG_X: u16 = 1;
pub const CONFIG_Y: u16 = 2;
pub const CONFIG_W: u16 = 4;
pub const CONFIG_H: u16 = 8;
pub const CONFIG_BORDER: u16 = 16;
pub const CONFIG_STACK: u16 = 64;

/// ConfigureWindow with x, y, width, height (and border 0); `raise` adds stack mode Above.
pub fn configure_window(win: u32, x: i32, y: i32, w: u32, h: u32, raise: bool) -> Vec<u8> {
    let mut mask = CONFIG_X | CONFIG_Y | CONFIG_W | CONFIG_H | CONFIG_BORDER;
    if raise {
        mask |= CONFIG_STACK;
    }
    let mut r = Req::new(CONFIGURE_WINDOW, 0).u32(win).u16(mask).u16(0).u32(x as u32).u32(y as u32).u32(w).u32(h).u32(0);
    if raise {
        r = r.u32(0);
    }
    r.done()
}

/// A ConfigureWindow carrying exactly a ConfigureRequest's fields (for windows the WM does not place).
#[allow(clippy::too_many_arguments)]
pub fn configure_as_asked(win: u32, mask: u16, x: i16, y: i16, w: u16, h: u16, border: u16, sibling: u32, stack: u8) -> Vec<u8> {
    let mut r = Req::new(CONFIGURE_WINDOW, 0).u32(win).u16(mask & 0x7f).u16(0);
    let fields = [(CONFIG_X, x as i32 as u32), (CONFIG_Y, y as i32 as u32), (CONFIG_W, w as u32), (CONFIG_H, h as u32),
        (CONFIG_BORDER, border as u32), (32, sibling), (CONFIG_STACK, stack as u32)];
    for (bit, v) in fields {
        if mask & bit != 0 {
            r = r.u32(v);
        }
    }
    r.done()
}

pub fn intern_atom(name: &str) -> Vec<u8> {
    Req::new(INTERN_ATOM, 0).u16(name.len() as u16).u16(0).bytes(name.as_bytes()).done()
}

pub fn get_atom_name(atom: u32) -> Vec<u8> {
    Req::new(GET_ATOM_NAME, 0).u32(atom).done()
}

pub fn change_property(win: u32, prop: u32, ty: u32, format: u8, data: &[u8]) -> Vec<u8> {
    let units = data.len() as u32 / (format as u32 / 8).max(1);
    Req::new(CHANGE_PROPERTY, 0).u32(win).u32(prop).u32(ty).u8(format).u8(0).u16(0).u32(units).bytes(data).done()
}

/// An empty Append to a property: changes nothing but makes the server send a PropertyNotify carrying its
/// current time (ICCCM 2.1: how a client without a recent event learns a timestamp).
pub fn touch_property(win: u32, prop: u32, ty: u32) -> Vec<u8> {
    Req::new(CHANGE_PROPERTY, 2).u32(win).u32(prop).u32(ty).u8(8).u8(0).u16(0).u32(0).done()
}

pub fn change_property32(win: u32, prop: u32, ty: u32, values: &[u32]) -> Vec<u8> {
    let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    change_property(win, prop, ty, 32, &data)
}

/// GetProperty of any type, up to `units` 4-byte units from the start.
pub fn get_property(delete: bool, win: u32, prop: u32, units: u32) -> Vec<u8> {
    Req::new(GET_PROPERTY, delete as u8).u32(win).u32(prop).u32(0).u32(0).u32(units).done()
}

pub fn set_selection_owner(owner: u32, selection: u32, time: u32) -> Vec<u8> {
    Req::new(SET_SELECTION_OWNER, 0).u32(owner).u32(selection).u32(time).done()
}

pub fn convert_selection(requestor: u32, selection: u32, target: u32, prop: u32, time: u32) -> Vec<u8> {
    Req::new(CONVERT_SELECTION, 0).u32(requestor).u32(selection).u32(target).u32(prop).u32(time).done()
}

pub fn send_event(dest: u32, mask: u32, event: &[u8; 32]) -> Vec<u8> {
    Req::new(SEND_EVENT, 0).u32(dest).u32(mask).bytes(event).done()
}

pub fn set_input_focus(win: u32, time: u32) -> Vec<u8> {
    // Revert to PointerRoot (1): a closed focused window leaves the focus with the pointer, as other WMs do.
    Req::new(SET_INPUT_FOCUS, 1).u32(win).u32(time).done()
}

pub fn query_extension(name: &str) -> Vec<u8> {
    Req::new(QUERY_EXTENSION, 0).u16(name.len() as u16).u16(0).bytes(name.as_bytes()).done()
}

pub fn kill_client(resource: u32) -> Vec<u8> {
    Req::new(KILL_CLIENT, 0).u32(resource).done()
}

/// An extension's QueryVersion (minor 0 for Composite, XFixes and X-Resource), versions as u32 pairs.
pub fn ext_query_version(major_op: u8, major: u32, minor: u32) -> Vec<u8> {
    Req::new(major_op, 0).u32(major).u32(minor).done()
}

/// Composite RedirectSubwindows (minor 2), update Manual (1): Xwayland gives each redirected top level a surface.
pub fn composite_redirect_subwindows(op: u8, win: u32) -> Vec<u8> {
    Req::new(op, 2).u32(win).u8(1).u8(0).u16(0).done()
}

/// XFixes SelectSelectionInput (minor 2): set-owner, window-destroy and client-close notices of `selection`.
pub fn xfixes_select_selection_input(op: u8, win: u32, selection: u32) -> Vec<u8> {
    Req::new(op, 2).u32(win).u32(selection).u32(7).done()
}

/// X-Resource QueryClientIds (minor 4) for the client owning `win`, asking its pid (mask LocalClientPID = 2).
pub fn xres_query_client_pid(op: u8, win: u32) -> Vec<u8> {
    Req::new(op, 4).u32(1).u32(win).u32(2).done()
}

/// The pid in a QueryClientIds reply (the first value of the first id with the pid mask), 0 when none.
pub fn xres_pid(reply: &[u8]) -> u32 {
    let n = u32_at(reply, 8) as usize;
    let mut at = 32;
    for _ in 0..n {
        let mask = u32_at(reply, at + 4);
        let len = u32_at(reply, at + 8) as usize;
        if mask == 2 && len >= 4 {
            return u32_at(reply, at + 12);
        }
        at += 12 + len;
        if at > reply.len() {
            break;
        }
    }
    0
}

/// A GetProperty reply: its type, format, the value bytes and how many bytes remain on the server.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Prop {
    pub ty: u32,
    pub format: u8,
    pub value: Vec<u8>,
    pub after: u32,
}

pub fn parse_property(reply: &[u8]) -> Prop {
    let format = reply.get(1).copied().unwrap_or(0);
    let n = u32_at(reply, 16) as usize * (format as usize / 8);
    Prop { ty: u32_at(reply, 8), format, after: u32_at(reply, 12), value: reply.get(32..32 + n).unwrap_or(&[]).to_vec() }
}

impl Prop {
    pub fn u32s(&self) -> Vec<u32> {
        if self.format != 32 {
            return Vec::new();
        }
        (0..self.value.len() / 4).map(|i| u32_at(&self.value, i * 4)).collect()
    }
}

/// A 32-byte event; `code` without the synthetic bit.
pub fn event(code: u8, fields: &[(usize, u32)], detail: u8) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0] = code;
    e[1] = detail;
    for (at, v) in fields {
        e[*at..*at + 4].copy_from_slice(&v.to_le_bytes());
    }
    e
}

/// A synthetic ConfigureNotify telling a client where its window is (ICCCM 4.1.5).
pub fn configure_notify(win: u32, x: i32, y: i32, w: u32, h: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0] = CONFIGURE_NOTIFY;
    e[4..8].copy_from_slice(&win.to_le_bytes());
    e[8..12].copy_from_slice(&win.to_le_bytes());
    e[16..18].copy_from_slice(&(x as i16).to_le_bytes());
    e[18..20].copy_from_slice(&(y as i16).to_le_bytes());
    e[20..22].copy_from_slice(&(w as u16).to_le_bytes());
    e[22..24].copy_from_slice(&(h as u16).to_le_bytes());
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_carry_their_length_in_units() {
        let r = intern_atom("WM_STATE");
        assert_eq!(r.len(), 16);
        assert_eq!(u16_at(&r, 2), 4);
        assert_eq!(&r[8..16], b"WM_STATE");
        let c = configure_window(7, -3, 4, 100, 50, true);
        assert_eq!(c.len() / 4, u16_at(&c, 2) as usize);
        assert_eq!(u16_at(&c, 8), 0x5f);
        assert_eq!(u32_at(&c, 12) as i32, -3);
    }

    #[test]
    fn a_configure_request_is_passed_on_field_by_field() {
        let r = configure_as_asked(9, CONFIG_W | CONFIG_H, 0, 0, 640, 480, 0, 0, 0);
        assert_eq!(r.len(), 20);
        assert_eq!(u32_at(&r, 12), 640);
        assert_eq!(u32_at(&r, 16), 480);
    }

    #[test]
    fn property_replies_and_xres_pids_are_read() {
        let mut b = vec![0u8; 40];
        b[0] = 1;
        b[1] = 32;
        b[8..12].copy_from_slice(&ATOM_ATOM.to_le_bytes());
        b[16..20].copy_from_slice(&2u32.to_le_bytes());
        b[32..36].copy_from_slice(&5u32.to_le_bytes());
        b[36..40].copy_from_slice(&6u32.to_le_bytes());
        let p = parse_property(&b);
        assert_eq!(p.u32s(), vec![5, 6]);
        let mut x = vec![0u8; 48];
        x[8..12].copy_from_slice(&1u32.to_le_bytes());
        x[36..40].copy_from_slice(&2u32.to_le_bytes());
        x[40..44].copy_from_slice(&4u32.to_le_bytes());
        x[44..48].copy_from_slice(&4242u32.to_le_bytes());
        assert_eq!(xres_pid(&x), 4242);
        assert_eq!(xres_pid(&x[..20]), 0);
    }

    #[test]
    fn short_or_lying_replies_give_nothing_and_never_panic() {
        // A reply cut before its value, and one whose length field claims more than it carries.
        let mut b = vec![0u8; 20];
        b[0] = 1;
        b[1] = 8;
        b[16..20].copy_from_slice(&1000u32.to_le_bytes());
        assert!(parse_property(&b).value.is_empty());
        let mut b = vec![0u8; 36];
        b[0] = 1;
        b[1] = 32;
        b[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        let p = parse_property(&b);
        assert!(p.value.is_empty() && p.u32s().is_empty());
        assert_eq!(parse_property(&[]), Prop::default());
        // An odd format has no 32-bit values; an X-Resource reply naming more ids than it holds.
        let p = Prop { format: 16, value: vec![1, 2, 3, 4], ..Default::default() };
        assert!(p.u32s().is_empty());
        let mut x = vec![0u8; 44];
        x[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
        x[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(xres_pid(&x), 0);
        let t = touch_property(5, 6, ATOM_STRING);
        assert_eq!((t[1], t.len(), u32_at(&t, 20)), (2, 24, 0));
    }

    #[test]
    fn a_refused_or_short_setup_is_an_error_not_a_panic() {
        assert!(parse_setup(&[0, 3, 11, 0, 0, 0, 1, 0, b'n', b'o', b'!', 0]).is_err());
        assert!(parse_setup(&[1, 0, 11, 0]).is_err());
    }
}
