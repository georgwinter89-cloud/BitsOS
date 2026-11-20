//! `bitsupdate`: the update on the text console (the Gaming-PC's safe mode, any console without a desktop), and
//! the desk's signing tool on its host (`cargo xtask update-release` runs it).
//!
//!   bitsupdate status            what is installed, what starts next
//!   bitsupdate check             is a newer version there (and what is new in it)
//!   bitsupdate install           download, check, then the real keyboard's yes, then install
//!   bitsupdate back | forward    the previous version at the next start, or the newest again
//!   bitsupdate frage             VB-1: "Version N läuft. Bestätigen?", answered by one key of the real keyboard
//!   bitsupdate token             reads the GitHub token from stdin (one line, no echo) and stores it
//!   bitsupdate keygen DIR        (desk host) a new key pair in DIR: release.key (0600), release.pub
//!   bitsupdate sign KEY FILE     (desk host) FILE.SIG next to FILE
//!   bitsupdate fingerprint FILE  (desk host) the public key's fingerprint
//!   bitsupdate manifest N DIR DATE [MIN]  (desk host) DIR/MANIFEST over the files in DIR
//!   bitsupdate manifest2 N DIR DATE [MIN] (desk host) DIR/MANIFEST2 with the parts: every bin.<name> file and
//!                                          every tree.<name>.<k>.tar.zst piece (renamed with the tree's hash)
//!   bitsupdate verify PUB DIR    (desk host) the set in DIR checked as BitsOS checks it
#![forbid(unsafe_code)]

use bitsupdate::flow::{self, Ctx};
use bitsupdate::{realkeys, sig, source, text, tty};

mod desk;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::time::Duration;

fn out(t: &str) {
    println!("{t}");
    let _ = std::io::stdout().flush();
}

fn die(t: &str) -> ! {
    eprintln!("bitsupdate: {t}");
    std::process::exit(1)
}

/// Georg's line, then the detail for the desk (who reads the console through MCP's screen tool) if there is one.
fn fail(f: &flow::Fail) -> ! {
    match f.detail() {
        Some(d) => die(&format!("{}\n  (für den Desk: {d})", f.text())),
        None => die(&f.text()),
    }
}

fn status() -> flow::Installed {
    flow::status().unwrap_or_else(|f| fail(&f))
}

fn show_status(i: &flow::Installed) {
    out(&format!("Installiert: {}", text::installed(i)));
    if i.pending {
        out(text::pending());
    }
    match i.pin {
        Some(p) => out(&format!("Beim nächsten Start: Generation {p} (Zurück gewählt).")),
        None => out(&format!("Beim nächsten Start: {}", i.next)),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match a.as_slice() {
        ["status"] => show_status(&status()),
        ["check"] => {
            let i = status();
            match flow::check(&Ctx::standard(), &i) {
                Ok(None) => out(text::CURRENT),
                Ok(Some(o)) => {
                    out(&format!("Neu: Version {} vom {}", o.release.version, text::german_date(&o.manifest.date)));
                    out(&o.notes);
                    if let Some(p) = &o.plan {
                        out(&format!("Es kommt: {}", text::comes(p)));
                    }
                }
                Err(f) => fail(&f),
            }
        }
        ["install"] => install(),
        ["frage"] => frage(),
        ["back"] => match flow::pin(true) {
            Ok(n) => out(&format!("Beim nächsten Start kommt Generation {n}.")),
            Err(f) => fail(&f),
        },
        ["forward"] => match flow::pin(false) {
            Ok(n) => out(&format!("Beim nächsten Start kommt wieder die neueste: Generation {n}.")),
            Err(f) => fail(&f),
        },
        ["token"] => {
            let line = tty::read_secret_line("GitHub-Schlüssel einfügen und Enter drücken (er erscheint nicht auf dem Bildschirm):")
                .unwrap_or_else(|t| die(&t));
            out(&format!("*** ({} Zeichen)", line.trim().chars().count()));
            match source::store_token(&source::token_dir(), &line) {
                Ok(()) => out("Gespeichert."),
                Err(t) => die(&t),
            }
        }
        ["keygen", dir] => keygen(dir),
        ["manifest", v, dir, date] => manifest(v, dir, date, None),
        ["manifest", v, dir, date, min] => manifest(v, dir, date, Some(min)),
        ["manifest2", v, dir, date] => desk::manifest2(v, dir, date, None).unwrap_or_else(|t| die(&t)),
        ["manifest2", v, dir, date, min] => desk::manifest2(v, dir, date, Some(min)).unwrap_or_else(|t| die(&t)),
        ["verify", pubfile, dir] => verify_dir(pubfile, dir),
        ["sign", key, file] => {
            let k = std::fs::read(key).unwrap_or_else(|e| die(&format!("{key}: {e}")));
            let msg = std::fs::read(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
            let s = sig::sign(&k, &msg).unwrap_or_else(|t| die(&t));
            std::fs::write(format!("{file}.SIG"), s).unwrap_or_else(|e| die(&format!("{file}.SIG: {e}")));
            out(&format!("signed {file} with {}", sig::fingerprint(&sig::public_of(&k).unwrap_or_else(|t| die(&t)))));
        }
        ["fingerprint", file] => {
            let t = std::fs::read_to_string(file).unwrap_or_else(|e| die(&format!("{file}: {e}")));
            let hex = t.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("");
            let b: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()).collect();
            let k: [u8; 32] = b.try_into().unwrap_or_else(|_| die("not a public key file"));
            out(&sig::fingerprint(&k));
        }
        _ => die("usage: bitsupdate status | check | install | frage | back | forward | token | keygen DIR | sign KEY FILE | fingerprint FILE | manifest N DIR DATE [MIN] | manifest2 N DIR DATE [MIN] | verify PUB DIR"),
    }
}

/// VB-1: the question at the prompt. J: Ja; Z (the German layout's, evdev KEY_Y): Zurück; Esc or N: later.
fn frage() {
    use bitsupdate::confirm::{self, Ask, Done};
    const KEY_J: u16 = 36;
    const KEY_Z: u16 = 21;
    let i = status();
    let q = match confirm::decide(&i) {
        Ask::Now(q) => q,
        Ask::Later(why) | Ask::Never(why) => return out(&format!("Keine Frage offen: {why}.")),
    };
    out(&confirm::question_text(&q));
    let keys: &[u16] = match q.back {
        Some(b) => {
            out(&format!("Taste J: Ja (Version {} bleibt).   Taste Z: Zurück zur vorherigen (Version {b}, BitsOS startet dann neu).   Esc: später.", q.version));
            &[KEY_J, KEY_Z]
        }
        None => {
            out(&format!("Taste J: Ja (Version {} bleibt).   Esc: später. {}", q.version, confirm::no_back()));
            &[KEY_J]
        }
    };
    out("Es zählt nur die echte Tastatur, ab zwei Sekunden nach dieser Frage.");
    match realkeys::one_key(Duration::from_secs(600), keys) {
        Err(why) => die(&format!("{why}: nichts geändert.")),
        Ok(None) => out("Später: nichts geändert. Ohne Bestätigung startet beim nächsten Mal wieder die vorherige Version."),
        Ok(Some(KEY_J)) => match confirm::yes(q.generation, Duration::from_secs(120)) {
            Ok(Done::Confirmed { .. }) => out(&confirm::confirmed(q.version)),
            Ok(Done::Writing) => out(&confirm::still_writing(q.version)),
            Err(d) => die(&format!("{}\n  (für den Desk: {d})", confirm::not_confirmed())),
        },
        Ok(Some(_)) => match flow::pin(true) {
            Ok(n) => {
                out(&confirm::going_back(&n));
                if let Err(d) = confirm::restart() {
                    die(&format!("Der Neustart klappte nicht; bitte selbst neu starten.\n  (für den Desk: {d})"));
                }
            }
            Err(f) => die(&format!("{}\n  (für den Desk: {})", confirm::back_failed(), f.detail().unwrap_or(""))),
        },
    }
}

fn install() {
    let cx = Ctx::standard();
    let i = status();
    show_status(&i);
    let o = match flow::check(&cx, &i) {
        Ok(None) => return out(text::CURRENT),
        Ok(Some(o)) => o,
        Err(f) => fail(&f),
    };
    out(&format!("Neu: Version {} vom {}", o.release.version, text::german_date(&o.manifest.date)));
    out(&o.notes);
    if let Some(p) = &o.plan {
        out(&format!("Es kommt: {}", text::comes(p)));
    }
    let b = flow::download(&cx, &i, &o, &mut |s| out(&text::step(s))).unwrap_or_else(|f| fail(&f));
    out(&format!("Version {} ist geprüft und von deinem Desk unterschrieben.", b.version));
    let a = realkeys::ask(Duration::from_secs(120), &mut |code| {
        out(&format!("Zum Installieren tippe auf der echten Tastatur: {code}   (Esc oder N: nicht installieren)"));
    });
    match a {
        realkeys::Answer::Yes => {}
        realkeys::Answer::No => return out(text::NOTHING),
        realkeys::Answer::Nothing(why) => die(&format!("{} ({why})", text::NOTHING)),
    }
    match flow::install(&b, &mut |s| out(&text::step(s))) {
        Ok(d) => out(&text::done(d.slot)),
        Err(f) => fail(&f),
    }
}

fn keygen(dir: &str) {
    let d = std::path::Path::new(dir);
    let key = d.join("release.key");
    if key.exists() {
        die(&format!("{} exists already: the desk has its key", key.display()));
    }
    std::fs::create_dir_all(d).unwrap_or_else(|e| die(&format!("{dir}: {e}")));
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
    let (pkcs8, public) = sig::keygen().unwrap_or_else(|t| die(&t));
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&key).unwrap_or_else(|e| die(&format!("{}: {e}", key.display())));
    f.write_all(&pkcs8).and_then(|_| f.sync_all()).unwrap_or_else(|e| die(&format!("{}: {e}", key.display())));
    std::fs::write(d.join("release.pub"), sig::public_file(&public)).unwrap_or_else(|e| die(&format!("release.pub: {e}")));
    out(&format!("new release key, fingerprint {}", sig::fingerprint(&public)));
}

/// `manifest VERSION DIR DATE [MIN]`: DIR/MANIFEST over the version's files in DIR (desk host).
fn manifest(version: &str, dir: &str, date: &str, min: Option<&str>) {
    use bitsupdate::manifest::{Entry, FILES, Manifest};
    let num = |s: &str| s.parse::<u32>().unwrap_or_else(|_| die(&format!("{s:?} is no number")));
    let d = std::path::Path::new(dir);
    let mut files = Vec::new();
    for (name, _) in FILES {
        let p = d.join(name);
        if !p.exists() {
            continue;
        }
        let b = std::fs::read(&p).unwrap_or_else(|e| die(&format!("{}: {e}", p.display())));
        files.push(Entry { name: name.to_string(), size: b.len() as u64, sha256: bitsupdate::verify::sha256(&b) });
    }
    let m = Manifest { version: num(version), date: date.to_string(), min_previous: min.map(num), files, bins: Vec::new(), trees: Vec::new() };
    let t = m.render();
    Manifest::parse(t.as_bytes()).unwrap_or_else(|e| die(&format!("the manifest would not read back: {e}")));
    std::fs::write(d.join("MANIFEST"), &t).unwrap_or_else(|e| die(&format!("MANIFEST: {e}")));
    out(&format!("manifest of version {} with {} files", m.version, m.files.len()));
}

/// `verify PUBFILE DIR`: the whole set as BitsOS checks it (desk host, before anything is uploaded).
fn verify_dir(pubfile: &str, dir: &str) {
    use bitsupdate::manifest::{MANIFEST, Manifest, SIGNATURE};
    let t = std::fs::read_to_string(pubfile).unwrap_or_else(|e| die(&format!("{pubfile}: {e}")));
    let hex = t.lines().map(str::trim).find(|l| !l.is_empty() && !l.starts_with('#')).unwrap_or("");
    let b: Vec<u8> = (0..hex.len() / 2).filter_map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()).collect();
    let key: [u8; 32] = b.try_into().unwrap_or_else(|_| die("not a public key file"));
    let d = std::path::Path::new(dir);
    let read = |n: &str| std::fs::read(d.join(n)).unwrap_or_else(|e| die(&format!("{n}: {e}")));
    let mb = read(MANIFEST);
    let version = Manifest::parse(&mb).unwrap_or_else(|e| die(&e)).version;
    let m = bitsupdate::verify::manifest(Some(&key), &mb, &read(SIGNATURE), version, 0, u32::MAX).unwrap_or_else(|w| die(&text::why(&w)));
    for e in &m.files {
        bitsupdate::verify::file(&m, &e.name, &read(&e.name)).unwrap_or_else(|w| die(&format!("{}: {w:?}", e.name)));
    }
    out(&format!("verified: version {} signed by {}, {} files", m.version, sig::fingerprint(&key), m.files.len()));
    if d.join(bitsupdate::manifest::MANIFEST2).exists() {
        out(&desk::verify2(&key, d, &m).unwrap_or_else(|t| die(&t)));
    }
}
