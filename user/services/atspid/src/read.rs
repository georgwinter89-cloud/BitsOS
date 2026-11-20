//! Reading one program's accessible tree over the bus into plain `Node`s. Siblings are read concurrently; what is
//! not on screen is left out (its subtree too), except the options of a choice, which `choose` needs by name.

use crate::atspi::{self, Bus, role, state};
use futures_util::future::join_all;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use zbus::OwnedObjectPath;

/// Bounds on one read: a web page can hold tens of thousands of objects.
pub const MAX_NODES: usize = 4000;
pub const MAX_DEPTH: u32 = 64;
/// Text of one field or label, in characters.
pub const MAX_TEXT: usize = 2000;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Node {
    pub path: String,
    pub role: u32,
    pub name: String,
    pub states: u64,
    pub ifaces: Vec<String>,
    /// Relative to the program's window.
    pub rect: Option<[i32; 4]>,
    pub actions: Vec<String>,
    pub text: Option<String>,
    pub value: Option<f64>,
    /// The object attributes of fields and regions (Chromium's `xml-roles`, `text-input-type`, `autocomplete`).
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Node>,
}

impl Node {
    pub fn has_iface(&self, short: &str) -> bool {
        self.ifaces.iter().any(|i| i.rsplit('.').next() == Some(short))
    }

    pub fn attr(&self, key: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

pub struct Reader<'a> {
    pub bus: &'a Bus,
    pub app: &'a str,
    left: AtomicUsize,
    cut: AtomicBool,
}

type Boxed<'b> = Pin<Box<dyn Future<Output = Option<Node>> + Send + 'b>>;

impl<'a> Reader<'a> {
    pub fn new(bus: &'a Bus, app: &'a str) -> Self {
        Reader { bus, app, left: AtomicUsize::new(MAX_NODES), cut: AtomicBool::new(false) }
    }

    /// Whether a bound cut the tree short.
    pub fn truncated(&self) -> bool {
        self.cut.load(Ordering::Relaxed)
    }

    /// The direct children of `path` as (bus name, path).
    pub async fn children(&self, path: &str) -> Vec<(String, String)> {
        let v: Vec<(String, OwnedObjectPath)> = self.bus.call(self.app, path, atspi::ACCESSIBLE, "GetChildren", &()).await.unwrap_or_default();
        v.into_iter().map(|(b, p)| (b, p.as_str().to_string())).collect()
    }

    pub async fn role(&self, path: &str) -> Option<u32> {
        self.bus.call(self.app, path, atspi::ACCESSIBLE, "GetRole", &()).await.ok()
    }

    /// Children, states and role for a check that must not guess (EINGABE2 focus proof): a failed call is an error.
    pub async fn look(&self, path: &str) -> Result<Vec<(String, String)>, zbus::Error> {
        let v: Vec<(String, OwnedObjectPath)> = self.bus.call(self.app, path, atspi::ACCESSIBLE, "GetChildren", &()).await?;
        Ok(v.into_iter().map(|(b, p)| (b, p.as_str().to_string())).collect())
    }

    pub async fn try_states(&self, path: &str) -> Result<u64, zbus::Error> {
        let w: Vec<u32> = self.bus.call(self.app, path, atspi::ACCESSIBLE, "GetState", &()).await?;
        Ok(atspi::state_mask(&w))
    }

    pub async fn states(&self, path: &str) -> u64 {
        let w: Vec<u32> = self.bus.call(self.app, path, atspi::ACCESSIBLE, "GetState", &()).await.unwrap_or_default();
        atspi::state_mask(&w)
    }

    pub async fn name(&self, path: &str) -> String {
        self.bus.prop::<String>(self.app, path, atspi::ACCESSIBLE, "Name").await.unwrap_or_default()
    }

    /// Role, states, name and interfaces of one object.
    async fn head(&self, path: &str) -> Option<Node> {
        let ifaces = async { self.bus.call::<_, Vec<String>>(self.app, path, atspi::ACCESSIBLE, "GetInterfaces", &()).await.unwrap_or_default() };
        let (role, states, name, ifaces) = futures_util::join!(self.role(path), self.states(path), self.name(path), ifaces);
        Some(Node { path: path.to_string(), role: role?, name, states, ifaces, ..Node::default() })
    }

    /// Rectangle, actions, text and value of `n`, as its interfaces offer them.
    async fn fill(&self, n: &mut Node) {
        let p = n.path.as_str();
        let rect = async {
            if !n.has_iface("Component") {
                return None;
            }
            let r: (i32, i32, i32, i32) = self.bus.call(self.app, p, atspi::COMPONENT, "GetExtents", &(atspi::COORD_WINDOW,)).await.ok()?;
            Some([r.0, r.1, r.2, r.3])
        };
        let actions = async {
            if !n.has_iface("Action") {
                return Vec::new();
            }
            let a: Vec<(String, String, String)> = self.bus.call(self.app, p, atspi::ACTION, "GetActions", &()).await.unwrap_or_default();
            a.into_iter().map(|(name, _, _)| name).collect()
        };
        let text = async {
            if n.role == role::PASSWORD_TEXT || !n.has_iface("Text") || !wants_text(n.role) {
                return None;
            }
            let t: String = self.bus.call(self.app, p, atspi::TEXT, "GetText", &(0i32, -1i32)).await.ok()?;
            Some(t.chars().take(MAX_TEXT).collect::<String>())
        };
        let value = async {
            if !n.has_iface("Value") {
                return None;
            }
            self.bus.prop::<f64>(self.app, p, atspi::VALUE, "CurrentValue").await.ok()
        };
        let attrs = async {
            if !wants_attrs(n.role) {
                return Vec::new();
            }
            let a: std::collections::HashMap<String, String> = self.bus.call(self.app, p, atspi::ACCESSIBLE, "GetAttributes", &()).await.unwrap_or_default();
            let mut v: Vec<(String, String)> = a.into_iter().filter(|(k, _)| ATTRS.contains(&k.as_str())).collect();
            v.sort();
            v
        };
        let (rect, actions, text, value, attrs) = futures_util::join!(rect, actions, text, value, attrs);
        (n.rect, n.actions, n.text, n.value, n.attrs) = (rect, actions, text, value, attrs);
    }

    /// One object without its children (what `act` checks before it does anything).
    pub async fn one(&self, path: &str) -> Option<Node> {
        let mut n = self.head(path).await?;
        self.fill(&mut n).await;
        Some(n)
    }

    /// One object and what is on screen below it. `hidden_ok`: read it even when it is not showing.
    pub fn node<'s>(&'s self, path: String, depth: u32, hidden_ok: bool) -> Boxed<'s> {
        Box::pin(async move {
            if depth > MAX_DEPTH || self.left.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1)).is_err() {
                self.cut.store(true, Ordering::Relaxed);
                return None;
            }
            let mut n = self.head(&path).await?;
            if !hidden_ok && depth > 0 && !atspi::has(n.states, state::SHOWING) {
                return None;
            }
            let (role, p) = (n.role, path.clone());
            let ((), kids) = futures_util::join!(self.fill(&mut n), self.children(&p));
            // Another bus name below this one is an embedded program of its own: not followed.
            let options = role == role::COMBO_BOX || hidden_ok;
            let reads = kids
                .into_iter()
                .filter(|(b, _)| b.is_empty() || b == self.app)
                .map(|(_, cp)| self.node(cp, depth + 1, options));
            n.children = join_all(reads).await.into_iter().flatten().collect();
            Some(n)
        })
    }
}

/// The attributes kept: what tells a search box or an address bar by its semantics (EINGABE2).
const ATTRS: &[&str] = &["xml-roles", "text-input-type", "autocomplete"];

fn wants_attrs(r: u32) -> bool {
    matches!(r, role::ENTRY | role::TEXT | role::COMBO_BOX | role::AUTOCOMPLETE | role::EDITBAR | role::LANDMARK | role::FORM)
}

/// Roles whose text content is their value or their name (fields, labels, paragraphs).
fn wants_text(r: u32) -> bool {
    matches!(
        r,
        role::ENTRY
            | role::TEXT
            | role::SPIN_BUTTON
            | role::AUTOCOMPLETE
            | role::EDITBAR
            | role::TERMINAL
            | role::LABEL
            | role::PARAGRAPH
            | role::STATIC
            | role::CAPTION
            | role::HEADING
            | role::COMBO_BOX
            | role::DATE_EDITOR
    )
}
