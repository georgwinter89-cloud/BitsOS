//! `program_start`: starts a program of the desktop's program library by its name, as a click on its tile does.
//! The desktop (bitscomp) registers a tool of this name with the kernel, but as a service of uid 1000 it may
//! not offer it on the local tools socket this assistant uses, so the assistant asks the desktop directly on
//! its control socket (`$XDG_RUNTIME_DIR/<wayland socket>.ctl`, the command `program-start NAME`). Not a
//! shell: the model sends a name, the desktop looks it up in its library and starts only that entry, as a
//! program of the desktop (it stays when the answer ends). The class is `other`: it asks Georg first.
//!
//! Any program of the user may already start programs on that socket (`bitscomp ctl spawn`), so it is no
//! secret door; the listener is still required to be this user's.

use super::{Class, ToolAnswer, ToolSpec};
use crate::json::{self, Json};
use std::io::{Read, Write};
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const NAME: &str = "program_start";
/// The longest name sent (bitscomp takes 200 bytes).
const NAME_MAX: usize = 200;
/// The desktop answers in its next turn; a start that takes longer has gone wrong there.
const WAIT: Duration = Duration::from_secs(10);
/// The longest answer read (a list of the library's names when a name fits several).
const ANSWER_MAX: u64 = 64 * 1024;

pub struct Desktop {
    path: PathBuf,
}

/// The control socket in `run`: the one `display` names, else the only `*.ctl` socket there (the assistant's
/// own start does not always carry WAYLAND_DISPLAY).
pub fn socket_in(run: &Path, display: Option<&str>) -> Option<PathBuf> {
    let is_socket = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.file_type().is_socket());
    if let Some(d) = display.filter(|d| !d.is_empty() && !d.contains('/')) {
        let p = run.join(format!("{d}.ctl"));
        return is_socket(&p).then_some(p);
    }
    let mut found = std::fs::read_dir(run).ok()?.filter_map(Result::ok).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ctl") && is_socket(p));
    let one = found.next()?;
    found.next().is_none().then_some(one)
}

impl Desktop {
    /// The session's desktop, when one runs for this user.
    pub fn find() -> Option<Desktop> {
        let uid = usys::geteuid().ok()?;
        let run = std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map_or_else(|| PathBuf::from(format!("/run/user/{uid}")), PathBuf::from);
        let display = std::env::var("WAYLAND_DISPLAY").ok();
        socket_in(&run, display.as_deref()).map(Desktop::at)
    }

    pub fn at(path: PathBuf) -> Desktop {
        Desktop { path }
    }

    pub fn spec() -> ToolSpec {
        let schema = r#"{"type":"object","properties":{"name":{"type":"string","minLength":1,"maxLength":200,"description":"name or id of a program in the library, for example \"Brave\" or \"Terminal\""}},"required":["name"],"additionalProperties":false}"#;
        ToolSpec {
            name: NAME.into(),
            description: "Starts a program on Georg's desktop: one entry of the desktop's program library, by its name or id (for example \
                          \"Brave\", \"Terminal\", \"Einstellungen\"). Not a command line: no options, no path. A name that fits several \
                          programs starts none and the answer lists them with their ids; a name that fits none lists what the library has."
                .into(),
            schema: json::parse(schema).unwrap_or(Json::Null),
            class: Class::Other,
        }
    }

    /// The entry the model's name means, by the desktop's own lookup: the arguments then carry its id
    /// (`name`, what starts) and its name (`shown`, what the question says), so the question names exactly the
    /// program a yes starts.
    pub fn settle(&self, args: &Json) -> Result<Json, String> {
        let found = self.say("program-find", args)?;
        let Some((id, shown)) = found.split_once('\t').filter(|(id, _)| !id.is_empty()) else {
            return Err(format!("the desktop's answer is not an entry: {found:?}"));
        };
        Ok(Json::obj().with("name", id).with("shown", shown))
    }

    pub fn call(&self, args: &Json) -> ToolAnswer {
        self.say("program-start", args)
    }

    /// One control command with the name in `args`, and the desktop's answer line.
    fn say(&self, command: &str, args: &Json) -> ToolAnswer {
        let name = args.str("name").map(str::trim).unwrap_or("");
        if name.is_empty() || name.len() > NAME_MAX || name.chars().any(char::is_control) {
            return Err("name must be the name or id of a program in the library (one line of text)".into());
        }
        let not_there = |e: std::io::Error| format!("the desktop does not answer ({e}); is a desktop session running?");
        let mut s = UnixStream::connect(&self.path).map_err(not_there)?;
        let uid = usys::geteuid().ok().and_then(|u| u32::try_from(u).ok());
        let peer = settingsd::peer::of(&s);
        if peer.uid.is_none() || peer.uid != uid {
            return Err(format!("{} is not this user's desktop (pid {} listens)", self.path.display(), peer.pid));
        }
        s.set_write_timeout(Some(WAIT)).and_then(|()| s.set_read_timeout(Some(WAIT))).map_err(not_there)?;
        s.write_all(format!("{command} {name}\n").as_bytes()).map_err(not_there)?;
        let mut answer = String::new();
        s.take(ANSWER_MAX).read_to_string(&mut answer).map_err(not_there)?;
        let answer = answer.trim();
        match answer.strip_prefix("error: ") {
            Some(why) => Err(why.to_string()),
            None if answer.is_empty() => Err("the desktop gave no answer".into()),
            None => Ok(answer.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("assist-desktop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_desktops_socket_is_the_named_one_or_the_only_one() {
        let d = dir("find");
        assert_eq!(socket_in(&d, None), None);
        let _a = UnixListener::bind(d.join("wayland-1.ctl")).unwrap();
        std::fs::write(d.join("plain.ctl"), "").unwrap();
        assert_eq!(socket_in(&d, None), Some(d.join("wayland-1.ctl")), "a file is no socket");
        assert_eq!(socket_in(&d, Some("wayland-1")), Some(d.join("wayland-1.ctl")));
        assert_eq!(socket_in(&d, Some("wayland-9")), None);
        assert_eq!(socket_in(&d, Some("../x")), Some(d.join("wayland-1.ctl")), "a display with a slash is not a name here");
        let _b = UnixListener::bind(d.join("wayland-2.ctl")).unwrap();
        assert_eq!(socket_in(&d, None), None, "two desktops: none is guessed");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_name_goes_to_the_desktop_and_its_answer_comes_back() {
        let d = dir("call");
        let path = d.join("wayland-1.ctl");
        let l = UnixListener::bind(&path).unwrap();
        let fake = std::thread::spawn(move || {
            let mut got = Vec::new();
            for answer in ["brave\tBrave", "started Brave (pid 42); its window appears when the program has drawn it", "error: no program of the library fits \"x\"", "kein Eintrag"] {
                let (mut s, _) = l.accept().unwrap();
                let mut line = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut s), &mut line).unwrap();
                got.push(line);
                s.write_all(format!("{answer}\n").as_bytes()).unwrap();
            }
            got
        });
        let desk = Desktop::at(path);
        let args = |name: &str| Json::obj().with("name", name);
        assert_eq!(desk.settle(&args("Browser")).map(|a| a.to_text()), Ok(r#"{"name":"brave","shown":"Brave"}"#.into()));
        assert!(desk.call(&args(" brave ")).unwrap().starts_with("started Brave (pid 42)"));
        assert_eq!(desk.call(&args("x")), Err("no program of the library fits \"x\"".into()));
        assert!(desk.settle(&args("y")).is_err_and(|e| e.contains("not an entry")));
        // What is no name never reaches the desktop.
        for bad in ["", "a\nspawn sh", &"x".repeat(NAME_MAX + 1)] {
            assert!(desk.call(&args(bad)).is_err(), "{bad:?}");
        }
        assert!(desk.call(&Json::obj()).is_err());
        assert_eq!(fake.join().unwrap(), ["program-find Browser\n", "program-start brave\n", "program-start x\n", "program-find y\n"]);
        assert_eq!(Desktop::spec().class, Class::Other);
        let _ = std::fs::remove_dir_all(&d);
    }
}
