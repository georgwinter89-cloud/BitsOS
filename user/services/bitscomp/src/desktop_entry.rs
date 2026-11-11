//! PK-L1a: a `.desktop` file reader after the Desktop Entry Specification 1.5
//! (specifications.freedesktop.org/desktop-entry-spec/1.5): groups, `Type=Application` only, the German name first,
//! `Exec` with its quoting and field codes. Pure (the caller reads the file and checks `TryExec`); a hostile file is
//! refused with a reason, never a panic: at most `MAX_BYTES` and `MAX_LINES`, UTF-8 only.

pub const MAX_BYTES: usize = 64 * 1024;
pub const MAX_LINES: usize = 4096;
/// The desktop name BitsOS answers to in `OnlyShowIn`/`NotShowIn`.
pub const DESKTOP: &str = "BitsOS";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    pub name: String,
    pub generic: String,
    pub comment: String,
    pub icon: String,
    /// `Exec` split into arguments, field codes expanded or dropped.
    pub exec: Vec<String>,
    pub try_exec: String,
    pub terminal: bool,
    pub no_display: bool,
    pub hidden: bool,
    pub categories: Vec<String>,
    pub keywords: Vec<String>,
    pub only_show_in: Vec<String>,
    pub not_show_in: Vec<String>,
    pub wm_class: String,
    /// `Actions` (read, not shown yet).
    pub actions: Vec<String>,
    /// What was read leniently (a bad key skipped, a boolean that is not true/false read as false): event lines.
    pub warnings: Vec<String>,
}

impl DesktopEntry {
    /// Why the entry is not shown in BitsOS's library, if it is not (`TryExec` is the caller's check).
    pub fn not_shown(&self) -> Option<&'static str> {
        if self.hidden {
            Some("Hidden=true")
        } else if self.no_display {
            Some("NoDisplay=true")
        } else if !self.only_show_in.is_empty() && !self.only_show_in.iter().any(|d| d == DESKTOP) {
            Some("OnlyShowIn without BitsOS")
        } else if self.not_show_in.iter().any(|d| d == DESKTOP) {
            Some("NotShowIn=BitsOS")
        } else {
            None
        }
    }
}

/// Parses a whole file's bytes; `location` is its path (`%k`).
pub fn parse(bytes: &[u8], location: &str) -> Result<DesktopEntry, String> {
    if bytes.len() > MAX_BYTES {
        return Err(format!("larger than {} KiB", MAX_BYTES / 1024));
    }
    let text = std::str::from_utf8(bytes).map_err(|e| format!("not UTF-8 (byte {})", e.valid_up_to()))?;
    let mut warnings = Vec::new();
    let main = main_group(text, &mut warnings)?;
    let get = |k: &str| main.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let local = |k: &str| {
        ["de_DE", "de"].iter().find_map(|l| get(&format!("{k}[{l}]"))).or_else(|| get(k)).map(unescape)
    };
    match get("Type") {
        Some("Application") => {}
        Some(t) => return Err(format!("Type={t}, not an application")),
        None => return Err("no Type".into()),
    }
    let name = local("Name").filter(|n| !n.trim().is_empty()).ok_or("no Name")?;
    // The spec says `true`/`false`; `False` (seen in the wild: Ghidra's file) is read as well; anything else is
    // false with a warning (one sloppy line does not take the program out of the library).
    let mut boolean = |k: &str| match get(k).map(str::to_ascii_lowercase).as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(v) => {
            warnings.push(format!("{k}={v} is not true or false: read as false"));
            false
        }
    };
    let (terminal, no_display, hidden) = (boolean("Terminal"), boolean("NoDisplay"), boolean("Hidden"));
    let list = |k: &str| get(k).map(split_list).unwrap_or_default();
    let mut e = DesktopEntry {
        generic: local("GenericName").unwrap_or_default(),
        comment: local("Comment").unwrap_or_default(),
        icon: get("Icon").map(unescape).unwrap_or_default(),
        try_exec: get("TryExec").map(unescape).unwrap_or_default(),
        terminal,
        no_display,
        hidden,
        categories: list("Categories"),
        keywords: ["Keywords[de_DE]", "Keywords[de]", "Keywords"].iter().find_map(|k| get(k)).map(split_list).unwrap_or_default(),
        only_show_in: list("OnlyShowIn"),
        not_show_in: list("NotShowIn"),
        wm_class: get("StartupWMClass").map(unescape).unwrap_or_default(),
        actions: list("Actions"),
        name,
        exec: Vec::new(),
        warnings,
    };
    let exec = get("Exec").ok_or("no Exec")?;
    e.exec = exec_args(&unescape(exec), &e.icon, &e.name, location)?;
    if e.exec.is_empty() {
        return Err("Exec names no program".into());
    }
    Ok(e)
}

/// The `[Desktop Entry]` group's keys (first value of a key given twice); every line is checked. A bad key is
/// skipped with a warning (real files have sloppy lines in action and X- groups); a broken structure refuses the file.
fn main_group(text: &str, warnings: &mut Vec<String>) -> Result<Vec<(String, String)>, String> {
    let mut group: Option<&str> = None;
    let mut seen_main = false;
    let mut keys = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        if n >= MAX_LINES {
            return Err(format!("more than {MAX_LINES} lines"));
        }
        let line = raw.trim_end_matches('\r');
        let t = line.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(g) = t.strip_prefix('[') {
            let g = g.trim_end().strip_suffix(']').ok_or_else(|| format!("line {}: an unclosed group header", n + 1))?;
            if g.contains(['[', ']']) || g.chars().any(char::is_control) {
                return Err(format!("line {}: a bad group name", n + 1));
            }
            if group.is_none() && g != "Desktop Entry" {
                return Err(format!("the first group is [{g}], not [Desktop Entry]"));
            }
            if g == "Desktop Entry" && seen_main {
                return Err("[Desktop Entry] twice".into());
            }
            seen_main |= g == "Desktop Entry";
            group = Some(g);
            continue;
        }
        let Some(g) = group else { return Err(format!("line {}: a key before any group", n + 1)) };
        let (k, v) = t.split_once('=').ok_or_else(|| format!("line {}: neither a key nor a group", n + 1))?;
        let k = k.trim_end();
        let base = k.split('[').next().unwrap_or("");
        let key_ok = !base.is_empty() && base.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
            && (base.len() == k.len() || k.ends_with(']'));
        if !key_ok {
            warnings.push(format!("line {}: a bad key {:?} skipped", n + 1, k.chars().take(64).collect::<String>()));
            continue;
        }
        if g == "Desktop Entry" && !keys.iter().any(|(key, _): &(String, String)| key == k) {
            keys.push((k.to_string(), v.trim_start().to_string()));
        }
    }
    if !seen_main {
        return Err("no [Desktop Entry] group".into());
    }
    Ok(keys)
}

/// The string escapes of a value (`\s \n \t \r \\`); an unknown escape keeps its backslash (the Exec layer reads it).
pub fn unescape(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// A `;`-separated list (`\;` is a literal `;`), empty parts left out.
pub fn split_list(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut it = v.chars();
    while let Some(c) = it.next() {
        match c {
            '\\' => match it.next() {
                Some(';') => cur.push(';'),
                Some(o) => {
                    cur.push('\\');
                    cur.push(o);
                }
                None => cur.push('\\'),
            },
            ';' => {
                let p = unescape(cur.trim());
                if !p.is_empty() {
                    out.push(p);
                }
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    let p = unescape(cur.trim());
    if !p.is_empty() {
        out.push(p);
    }
    out
}

/// `Exec` (string escapes already undone) into arguments: double quotes with `\" \` \$ \\` inside them, the field
/// codes (`%f %F %u %U` and the deprecated `%d %D %n %N %v %m` dropped, `%i` = `--icon ICON`, `%c` = the name, `%k`
/// = the file's location, `%%` = `%`; inside quotes too, as glib reads them); an unknown code outside quotes or an
/// unterminated quote is refused.
pub fn exec_args(exec: &str, icon: &str, name: &str, location: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    // A word exists once a quote opened it or a character went in (`""` is an empty argument).
    let mut word = false;
    // Whether the word was only a dropped field code (then no argument is left of it).
    let mut only_code = false;
    let mut it = exec.chars().peekable();
    let finish = |out: &mut Vec<String>, cur: &mut String, word: &mut bool, only_code: &mut bool| {
        if *word && !(*only_code && cur.is_empty()) {
            out.push(std::mem::take(cur));
        }
        cur.clear();
        *word = false;
        *only_code = false;
    };
    while let Some(c) = it.next() {
        match c {
            ' ' | '\t' | '\n' => finish(&mut out, &mut cur, &mut word, &mut only_code),
            '"' => {
                let (mut any, mut dropped) = (false, false);
                loop {
                    match it.next() {
                        None => return Err("Exec: an unterminated quote".into()),
                        Some('"') => break,
                        Some('\\') => match it.next() {
                            Some(e @ ('"' | '`' | '$' | '\\')) => {
                                cur.push(e);
                                any = true;
                            }
                            Some(o) => {
                                cur.push('\\');
                                cur.push(o);
                                any = true;
                            }
                            None => return Err("Exec: an unterminated quote".into()),
                        },
                        // The spec forbids field codes inside quotes; real files have them (`sh -c "x %U"`), and
                        // glib reads them there too: dropped or expanded as outside, an unknown one kept as it is.
                        Some('%') => match it.peek().copied() {
                            Some('%') => {
                                it.next();
                                cur.push('%');
                                any = true;
                            }
                            Some('f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm') => {
                                it.next();
                                dropped = true;
                            }
                            Some('i') => {
                                it.next();
                                if !icon.is_empty() {
                                    cur.push_str("--icon ");
                                    cur.push_str(icon);
                                    any = true;
                                }
                            }
                            Some('c') => {
                                it.next();
                                cur.push_str(name);
                                any = true;
                            }
                            Some('k') => {
                                it.next();
                                cur.push_str(location);
                                any = true;
                            }
                            _ => {
                                cur.push('%');
                                any = true;
                            }
                        },
                        Some(o) => {
                            cur.push(o);
                            any = true;
                        }
                    }
                }
                // `""` is an empty argument; `"%U"` alone leaves none, like `%U` outside quotes.
                if any || !dropped {
                    only_code = false;
                } else {
                    only_code = !word || only_code;
                }
                word = true;
            }
            '%' => {
                let code = it.next().ok_or("Exec: a lone % at the end")?;
                match code {
                    '%' => {
                        cur.push('%');
                        word = true;
                    }
                    'f' | 'F' | 'u' | 'U' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm' => {
                        only_code = !word || only_code;
                        word = true;
                    }
                    'i' => {
                        if !icon.is_empty() {
                            finish(&mut out, &mut cur, &mut word, &mut only_code);
                            out.push("--icon".into());
                            out.push(icon.into());
                        }
                    }
                    'c' => {
                        cur.push_str(name);
                        word = true;
                    }
                    'k' => {
                        cur.push_str(location);
                        word = true;
                    }
                    o => return Err(format!("Exec: an unknown field code %{o}")),
                }
            }
            '\\' => {
                // Outside quotes the spec wants reserved characters quoted; a backslash keeps the next one as is.
                if let Some(n) = it.next() {
                    cur.push(n);
                }
                word = true;
                only_code = false;
            }
            o => {
                cur.push(o);
                word = true;
                only_code = false;
            }
        }
    }
    finish(&mut out, &mut cur, &mut word, &mut only_code);
    Ok(out)
}
