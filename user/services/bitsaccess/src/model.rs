//! One entry of the access log and how the page names its parts.

use mcpreg::Json;

/// A merged entry: one program, one kind, one thing, over a span of time (unix ms).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub t0: u64,
    pub t1: u64,
    pub exe: String,
    pub pid: u32,
    pub uid: u32,
    /// read write create unlink rename exec device connect listen accept netclose setid caps ptrace
    pub kind: String,
    /// The folder, the device node, the peer's address, the program, the capability bits or the target pid.
    pub what: String,
    /// Files: a file name in the folder (the last one seen).
    pub name: String,
    pub proto: String,
    pub port: u16,
    /// A device opened for writing.
    pub write: bool,
    pub n: u64,
    pub tx: u64,
    pub rx: u64,
    /// The first time this program touched this kind of thing (a private folder, a device class, a host, a right).
    pub new: bool,
}

pub const FILE_KINDS: [&str; 5] = ["read", "write", "create", "unlink", "rename"];
pub const NET_KINDS: [&str; 4] = ["connect", "listen", "accept", "netclose"];
pub const RIGHT_KINDS: [&str; 3] = ["setid", "caps", "ptrace"];

impl Entry {
    pub fn is_file(&self) -> bool {
        FILE_KINDS.contains(&self.kind.as_str())
    }
    pub fn is_net(&self) -> bool {
        NET_KINDS.contains(&self.kind.as_str())
    }
    pub fn is_right(&self) -> bool {
        RIGHT_KINDS.contains(&self.kind.as_str())
    }

    /// What the store merges a day's entries by.
    pub fn key(&self) -> (String, String, String, String, u16) {
        (self.exe.clone(), self.kind.clone(), self.what.clone(), self.proto.clone(), self.port)
    }

    /// Folds `o` (the same thing) into this entry.
    pub fn absorb(&mut self, o: &Entry) {
        self.t0 = self.t0.min(o.t0);
        if o.t1 >= self.t1 {
            self.t1 = o.t1;
            self.pid = o.pid;
            self.uid = o.uid;
            if !o.name.is_empty() {
                self.name = o.name.clone();
            }
        }
        self.n += o.n;
        self.tx += o.tx;
        self.rx += o.rx;
        self.write |= o.write;
        self.new |= o.new;
    }

    pub fn to_json(&self) -> Json {
        let num = |v: u64| Json::Num(v as f64);
        let mut v = vec![("t0", num(self.t0)), ("t1", num(self.t1)), ("exe", Json::str(&self.exe)), ("pid", num(self.pid as u64)), ("uid", num(self.uid as u64)), ("kind", Json::str(&self.kind)), ("what", Json::str(&self.what))];
        if !self.name.is_empty() {
            v.push(("name", Json::str(&self.name)));
        }
        if !self.proto.is_empty() {
            v.push(("proto", Json::str(&self.proto)));
            v.push(("port", num(self.port as u64)));
        }
        if self.write {
            v.push(("write", Json::Bool(true)));
        }
        v.push(("n", num(self.n)));
        if self.tx > 0 || self.rx > 0 {
            v.push(("tx", num(self.tx)));
            v.push(("rx", num(self.rx)));
        }
        if self.new {
            v.push(("new", Json::Bool(true)));
        }
        Json::obj(v)
    }

    pub fn from_json(j: &Json) -> Option<Entry> {
        let s = |k: &str| j.get(k).and_then(Json::as_str).unwrap_or("").to_string();
        let u = |k: &str| j.get(k).and_then(Json::as_u64).unwrap_or(0);
        Some(Entry {
            t0: j.get("t0")?.as_u64()?,
            t1: j.get("t1")?.as_u64()?,
            exe: s("exe"),
            pid: u("pid") as u32,
            uid: u("uid") as u32,
            kind: j.get("kind")?.as_str()?.to_string(),
            what: s("what"),
            name: s("name"),
            proto: s("proto"),
            port: u("port") as u16,
            write: j.get("write").and_then(Json::as_bool).unwrap_or(false),
            n: u("n"),
            tx: u("tx"),
            rx: u("rx"),
            new: j.get("new").and_then(Json::as_bool).unwrap_or(false),
        })
    }
}

/// The device classes the page groups by: (class, German name).
pub const DEVICE_CLASSES: [(&str, &str); 8] = [
    ("mikrofon", "Mikrofon"),
    ("kamera", "Kamera"),
    ("ton", "Ton"),
    ("eingabe", "Eingabe"),
    ("laufwerk", "Laufwerke"),
    ("grafik", "Grafik"),
    ("usb", "USB"),
    ("andere", "Andere Geräte"),
];

/// The class of a device node by its path (Linux's names, which V2 keeps).
pub fn device_class(path: &str) -> &'static str {
    let name = path.rsplit('/').next().unwrap_or("");
    if let Some(pcm) = path.strip_prefix("/dev/snd/pcm") {
        return if pcm.ends_with('c') { "mikrofon" } else { "ton" };
    }
    if path.starts_with("/dev/snd/") {
        return "ton";
    }
    if name.starts_with("video") || name.starts_with("media") {
        return "kamera";
    }
    if path.starts_with("/dev/input/") || name.starts_with("hidraw") || name == "uinput" {
        return "eingabe";
    }
    if path.starts_with("/dev/dri/") || name.starts_with("fb") {
        return "grafik";
    }
    if path.starts_with("/dev/bus/usb/") {
        return "usb";
    }
    if ["sd", "nvme", "mmcblk", "loop", "dm-", "vd", "sr"].iter().any(|p| name.starts_with(p)) {
        return "laufwerk";
    }
    "andere"
}

pub fn device_name(class: &str) -> &'static str {
    DEVICE_CLASSES.iter().find(|c| c.0 == class).map_or("Andere Geräte", |c| c.1)
}

/// The private root `folder` lies in (context.private_roots), if any.
pub fn private_root<'a>(folder: &str, roots: &'a [String]) -> Option<&'a str> {
    roots.iter().map(|r| r.trim_end_matches('/')).find(|r| !r.is_empty() && (folder == *r || folder.strip_prefix(*r).is_some_and(|rest| rest.starts_with('/')))).map(|r| r as &str)
}

/// BitsOS's own programs: started by root, or shipped in /bin, /sbin or /usr/lib/bitsos.
pub fn is_system(exe: &str, uid: u32) -> bool {
    uid == 0 || is_system_path(exe)
}

/// Shipped with BitsOS: in /bin, /sbin or /usr/lib/bitsos (or the kernel's own, without a path).
pub fn is_system_path(exe: &str) -> bool {
    exe.is_empty() || ["/bin/", "/sbin/", "/usr/lib/bitsos/"].iter().any(|p| exe.starts_with(p))
}

/// A program's name as the page shows it: the file name of its path.
pub fn program_name(exe: &str) -> &str {
    match exe.rsplit('/').next() {
        Some(n) if !n.is_empty() => n,
        _ => "unbekanntes Programm",
    }
}

/// A peer in plain words: this machine, the home network, or the address.
pub fn host_words(addr: &str) -> String {
    if addr == "127.0.0.1" || addr == "0:0:0:0:0:0:0:1" || addr.starts_with("127.") {
        return "dieser Rechner".into();
    }
    if addr == "0.0.0.0" || addr == "0:0:0:0:0:0:0:0" {
        return "alle Netze".into();
    }
    let o: Vec<u8> = addr.split('.').filter_map(|x| x.parse().ok()).collect();
    let home = matches!(o.as_slice(), [192, 168, ..] | [10, ..]) || matches!(o.as_slice(), [172, b, ..] if (16..32).contains(b)) || addr.starts_with("fe80:");
    if home { format!("{addr} (Heimnetz)") } else { addr.to_string() }
}

/// A port in plain words where it is a well-known one.
pub fn port_words(proto: &str, port: u16) -> String {
    let what = match (proto, port) {
        (_, 443) => "sicheres Web",
        (_, 80) => "Web",
        (_, 53) => "Namen im Netz",
        ("udp", 67 | 68) => "Netzadresse",
        ("udp", 123) => "Uhrzeit",
        (_, 22) => "Fernanmeldung",
        _ => "",
    };
    if what.is_empty() { format!("Port {port}") } else { format!("Port {port}, {what}") }
}
