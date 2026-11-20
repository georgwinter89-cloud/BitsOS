//! A world with helper scripts: what the tests of provider kind `program` share.

use super::World;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// A world with helper scripts in a folder of its own.
pub struct Stage {
    pub w: World,
    pub dir: PathBuf,
}

impl Stage {
    /// At the level ask: what a helper's calls ask is what these tests are about. The default is auto
    /// (DECISIONS 18.11.); tightening is anyone's, so this is a plain set.
    pub fn new(name: &str) -> Stage {
        let w = World::new(name);
        w.set("ai.level", "ask");
        let dir = w.root.join("helpers");
        std::fs::create_dir_all(&dir).unwrap();
        Stage { w, dir }
    }

    /// Writes `body` as an executable sh script. Through a shell: this test process never holds the file
    /// open for writing, which would make starting it fail now and then ("text file busy") while another
    /// test thread starts a program.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join(name);
        let mut sh = Command::new("sh").args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"]).arg(&path).stdin(Stdio::piped()).spawn().unwrap();
        sh.stdin.take().unwrap().write_all(format!("#!/bin/sh\ncd '{}'\n{body}\n", self.dir.display()).as_bytes()).unwrap();
        assert!(sh.wait().unwrap().success());
        path
    }

    /// A provider of kind program, set up the way Georg would: no model, no key.
    pub fn provider(&self, id: &str, address: &Path, place: &str) {
        let w = &self.w;
        let mut ids = assist::config::parse_ids(w.value("ai.providers").as_str().unwrap_or(""));
        if !ids.iter().any(|i| i == id) {
            ids.push(id.to_string());
            w.set("ai.providers", ids.join(", "));
        }
        w.set(&format!("ai.provider.{id}.kind"), "program");
        w.set(&format!("ai.provider.{id}.address"), address.to_str().unwrap());
        w.set(&format!("ai.provider.{id}.place"), place);
    }

    /// The chat's provider is a helper with this script.
    pub fn helper(&self, body: &str) -> PathBuf {
        let path = self.script("helper", body);
        self.provider("h", &path, "local");
        self.w.set("ai.role.chat", "h");
        path
    }

    pub fn file(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.join(name)).unwrap_or_default()
    }

    /// True while the process whose number the script wrote into `name` is there (a zombie is not).
    pub fn runs(&self, name: &str) -> bool {
        let pid = self.file(name).trim().to_string();
        assert!(!pid.is_empty(), "the helper never wrote {name}");
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.rsplit(") ").next().is_some_and(|rest| rest.starts_with('Z')))
    }

    pub fn gone(&self, name: &str) -> bool {
        (0..150).any(|_| {
            let gone = !self.runs(name);
            if !gone {
                std::thread::sleep(Duration::from_millis(20));
            }
            gone
        })
    }
}

/// The first lines of a helper script of version 2: the request is read (one line; stdin then ends) and kept
/// in the file `request`, and `ask '{"ask":...}'` puts a question to the assistant and prints its answer line.
pub fn asking() -> String {
    format!(
        "read -r request\nprintf '%s\\n' \"$request\" > request\nbell=$(printf '%s' \"$request\" | sed -n 's/.*\"bell\":\"\\([^\"]*\\)\".*/\\1/p')\nask() {{ printf '%s\\n' \"$1\" | '{}' --ask --bell \"$bell\"; }}\n",
        env!("CARGO_BIN_EXE_assist")
    )
}

/// One request on a helper's tool socket (contract version 4), its answer line on stdout. With a third word:
/// connect at once, ignore SIGTERM, send only after that many seconds (a call that comes after the answer).
const CLIENT: &str = r#"import signal, socket, sys, time
sock, line = sys.argv[1], sys.argv[2]
late = float(sys.argv[3]) if len(sys.argv) > 3 else 0
if late:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
try:
    s = socket.socket(socket.AF_UNIX)
    s.connect(sock)
    time.sleep(late)
    s.sendall((line + "\n").encode())
    print(s.makefile("r").readline().strip(), flush=True)
except OSError as e:
    print("", flush=True)
    print(e, file=sys.stderr)
"#;

/// The first lines of a helper of version 4: the request is kept in `request` (the n-th in `request.n`),
/// `$sock` is the tool socket and `call '{...}'` prints the answer line of one request. The socket is spoken to
/// by a few lines of Python: in these tests the listener is the test process, not `assist`, so `assist --call`
/// would rightly refuse it.
pub fn calling(s: &Stage) -> String {
    std::fs::write(s.dir.join("client.py"), CLIENT).unwrap();
    "read -r request\nprintf '%s\\n' \"$request\" > request\nn=$(cat calls 2>/dev/null || echo 0); n=$((n+1)); echo $n > calls; cp request request.$n\nsock=$(printf '%s' \"$request\" | sed -n 's/.*\"tool_socket\":\"\\([^\"]*\\)\".*/\\1/p')\ncall() { python3 -I client.py \"$sock\" \"$1\"; }\n".to_string()
}
