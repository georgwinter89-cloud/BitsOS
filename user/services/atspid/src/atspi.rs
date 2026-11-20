//! AT-SPI2 on the wire: the interface names, the role and state numbers this bridge needs (atspi-constants.h of
//! at-spi2-core 2.62, `AtspiRole` and `AtspiStateType`) and the one call helper every read and act goes through.

use std::sync::Arc;
use zbus::{Connection, DynamicType, OwnedVariant};
use zruntime::lock::Semaphore;

pub const ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
pub const COMPONENT: &str = "org.a11y.atspi.Component";
pub const ACTION: &str = "org.a11y.atspi.Action";
pub const TEXT: &str = "org.a11y.atspi.Text";
pub const EDITABLE_TEXT: &str = "org.a11y.atspi.EditableText";
pub const SELECTION: &str = "org.a11y.atspi.Selection";
pub const VALUE: &str = "org.a11y.atspi.Value";
pub const SOCKET: &str = "org.a11y.atspi.Socket";
pub const REGISTRY: &str = "org.a11y.atspi.Registry";
pub const PROPERTIES: &str = "org.freedesktop.DBus.Properties";

/// Where an application puts its root, and where the registry answers `Embed` (at-spi2-core's ATSPI_DBUS_PATH_ROOT).
pub const ROOT_PATH: &str = "/org/a11y/atspi/accessible/root";
/// ATSPI_DBUS_PATH_REGISTRY: `GetRegisteredEvents` and friends.
pub const REGISTRY_PATH: &str = "/org/a11y/atspi/registry";
/// The device event controller beside the registry.
pub const DEC_PATH: &str = "/org/a11y/atspi/registry/deviceeventcontroller";
/// The registry's well-known name on the accessibility bus.
pub const REGISTRY_NAME: &str = "org.a11y.atspi.Registry";

pub mod role {
    pub const ACCELERATOR_LABEL: u32 = 1;
    pub const ALERT: u32 = 2;
    pub const ARROW: u32 = 4;
    pub const CANVAS: u32 = 6;
    pub const CHECK_BOX: u32 = 7;
    pub const CHECK_MENU_ITEM: u32 = 8;
    pub const COMBO_BOX: u32 = 11;
    pub const DATE_EDITOR: u32 = 12;
    pub const DIAL: u32 = 15;
    pub const DIALOG: u32 = 16;
    pub const DRAWING_AREA: u32 = 18;
    pub const FRAME: u32 = 23;
    pub const ICON: u32 = 26;
    pub const IMAGE: u32 = 27;
    pub const LABEL: u32 = 29;
    pub const LIST: u32 = 31;
    pub const LIST_ITEM: u32 = 32;
    pub const MENU: u32 = 33;
    pub const MENU_BAR: u32 = 34;
    pub const MENU_ITEM: u32 = 35;
    pub const PAGE_TAB: u32 = 37;
    pub const PAGE_TAB_LIST: u32 = 38;
    pub const PASSWORD_TEXT: u32 = 40;
    pub const POPUP_MENU: u32 = 41;
    pub const PROGRESS_BAR: u32 = 42;
    pub const PUSH_BUTTON: u32 = 43;
    pub const RADIO_BUTTON: u32 = 44;
    pub const RADIO_MENU_ITEM: u32 = 45;
    pub const SCROLL_BAR: u32 = 48;
    pub const SLIDER: u32 = 51;
    pub const SPIN_BUTTON: u32 = 52;
    pub const STATUS_BAR: u32 = 54;
    pub const TABLE: u32 = 55;
    pub const TABLE_CELL: u32 = 56;
    pub const TEAROFF_MENU_ITEM: u32 = 59;
    pub const TERMINAL: u32 = 60;
    pub const TEXT: u32 = 61;
    pub const TOGGLE_BUTTON: u32 = 62;
    pub const TOOL_BAR: u32 = 63;
    pub const TOOL_TIP: u32 = 64;
    pub const TREE: u32 = 65;
    pub const TREE_TABLE: u32 = 66;
    pub const WINDOW: u32 = 69;
    pub const PARAGRAPH: u32 = 73;
    pub const APPLICATION: u32 = 75;
    pub const AUTOCOMPLETE: u32 = 76;
    pub const EDITBAR: u32 = 77;
    pub const EMBEDDED: u32 = 78;
    pub const ENTRY: u32 = 79;
    pub const CAPTION: u32 = 81;
    pub const DOCUMENT_FRAME: u32 = 82;
    pub const HEADING: u32 = 83;
    pub const FORM: u32 = 87;
    pub const LINK: u32 = 88;
    pub const TREE_ITEM: u32 = 91;
    pub const DOCUMENT_SPREADSHEET: u32 = 92;
    pub const DOCUMENT_PRESENTATION: u32 = 93;
    pub const DOCUMENT_TEXT: u32 = 94;
    pub const DOCUMENT_WEB: u32 = 95;
    pub const DOCUMENT_EMAIL: u32 = 96;
    pub const LIST_BOX: u32 = 98;
    pub const GROUPING: u32 = 99;
    pub const IMAGE_MAP: u32 = 100;
    pub const NOTIFICATION: u32 = 101;
    pub const LEVEL_BAR: u32 = 103;
    pub const ARTICLE: u32 = 109;
    pub const LANDMARK: u32 = 110;
    pub const STATIC: u32 = 116;
    pub const DESCRIPTION_LIST: u32 = 121;
    pub const PUSH_BUTTON_MENU: u32 = 129;
    pub const SWITCH: u32 = 130;
    pub const VIDEO: u32 = 107;
}

pub mod state {
    pub const ACTIVE: u32 = 1;
    pub const BUSY: u32 = 3;
    pub const CHECKED: u32 = 4;
    pub const EDITABLE: u32 = 7;
    pub const ENABLED: u32 = 8;
    pub const EXPANDED: u32 = 10;
    pub const FOCUSABLE: u32 = 11;
    pub const FOCUSED: u32 = 12;
    pub const MULTI_LINE: u32 = 17;
    pub const PRESSED: u32 = 20;
    pub const SELECTED: u32 = 23;
    pub const SENSITIVE: u32 = 24;
    pub const SHOWING: u32 = 25;
    pub const SINGLE_LINE: u32 = 26;
    pub const VISIBLE: u32 = 30;
    pub const READ_ONLY: u32 = 43;
}

/// AtspiCoordType: relative to the program's own window (Wayland gives a program no screen position).
pub const COORD_WINDOW: u32 = 1;
/// AtspiScrollType ANYWHERE.
pub const SCROLL_ANYWHERE: u32 = 6;

/// The state set as AT-SPI sends it (two 32-bit words) into one mask.
pub fn state_mask(words: &[u32]) -> u64 {
    words.iter().take(2).enumerate().fold(0, |m, (i, w)| m | (u64::from(*w) << (32 * i)))
}

pub fn has(states: u64, s: u32) -> bool {
    s < 64 && states & (1 << s) != 0
}

/// A connection to the accessibility bus with a bound on calls in flight: a large tree read concurrently must
/// not queue thousands of calls at one program.
#[derive(Clone)]
pub struct Bus {
    pub conn: Connection,
    gate: Arc<Semaphore>,
}

impl Bus {
    pub fn new(conn: Connection) -> Self {
        Bus { conn, gate: Arc::new(Semaphore::new(48)) }
    }

    pub async fn call<B, R>(&self, dest: &str, path: &str, iface: &str, method: &str, body: &B) -> zbus::Result<R>
    where
        B: serde::Serialize + DynamicType,
        R: zbus::Type + serde::de::DeserializeOwned,
    {
        let _permit = self.gate.acquire().await;
        let m = self.conn.call_method(Some(dest), path, Some(iface), method, body).await?;
        m.body().deserialize::<R>()
    }

    pub async fn prop<R>(&self, dest: &str, path: &str, iface: &str, name: &str) -> zbus::Result<R>
    where
        R: TryFrom<OwnedVariant>,
    {
        let v: OwnedVariant = self.call(dest, path, PROPERTIES, "Get", &(iface, name)).await?;
        R::try_from(v).map_err(|_| zbus::Error::Failure(format!("property {iface}.{name} has another type")))
    }
}
