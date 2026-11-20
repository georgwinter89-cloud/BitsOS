//! bitsrun (PK-3): starts a program in its view (plan/PACKAGES.md 6.7, plan/spec/program-library.md D8). `plan` decides the view
//! from facts (pure, host-tested), `facts` reads them from the file system, `bwrap` turns a plan into bubblewrap's
//! arguments (the executor until bitsrun runs its plan with usys's unshare/pivot_root/umount2).
#![forbid(unsafe_code)]

pub mod browser;
pub mod browser_run;
pub mod bwrap;
pub mod facts;
pub mod plan;

/// One B6 event line on stderr, in bitspkg's format (`bitsrun: event=NAME k=v ...`).
pub fn event(name: &str, kv: &[(&str, &str)]) {
    eprintln!("{}", event_line(name, kv));
}

pub fn event_line(name: &str, kv: &[(&str, &str)]) -> String {
    let mut l = format!("bitsrun: event={name}");
    for (k, v) in kv {
        if v.is_empty() || v.contains([' ', '"', '=']) {
            l.push_str(&format!(" {k}=\"{}\"", v.replace('"', "'")));
        } else {
            l.push_str(&format!(" {k}={v}"));
        }
    }
    l
}
