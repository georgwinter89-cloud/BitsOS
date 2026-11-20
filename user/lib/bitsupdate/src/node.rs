//! The kernel's install node `/dev/bitsos/update` (crates/kexec/src/install/update_node.rs): one command per
//! write, the answer (one JSON line) read back with read(2). Root only.

use mcp_wire::json::Json;
use std::io::{Read, Write};
use std::path::PathBuf;

pub const NODE: &str = "/dev/bitsos/update";
const CHUNK: usize = 1 << 20;

pub fn path() -> PathBuf {
    std::env::var_os("BITSUPDATE_NODE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(NODE))
}

pub struct Node {
    f: std::fs::File,
}

impl Node {
    /// The error as the open gave it: a refusal (not root, or a process MCP's run started) is Georg's own line
    /// (`flow::node`).
    pub fn open() -> std::io::Result<Node> {
        let f = std::fs::OpenOptions::new().read(true).write(true).open(path())?;
        Ok(Node { f })
    }

    /// The answer to the last command: one line (the node is a stream; the line ends in a newline).
    fn answer(&mut self) -> Result<Json, String> {
        let mut buf = vec![0u8; 256 * 1024];
        let mut got = 0;
        while got < buf.len() && !buf[..got].ends_with(b"\n") {
            let n = self.f.read(&mut buf[got..]).map_err(|e| format!("reading the answer: {e}"))?;
            if n == 0 {
                break;
            }
            got += n;
        }
        let t = String::from_utf8_lossy(&buf[..got]);
        Json::parse(t.trim()).map_err(|_| format!("the kernel answered no JSON: {}", t.trim()))
    }

    pub fn command(&mut self, line: &str) -> Result<Json, String> {
        self.f.write_all(line.as_bytes()).map_err(|e| format!("{line}: {e}"))?;
        self.answer()
    }

    /// The kernel's reason in a refusing answer.
    fn why(a: &Json) -> String {
        ["refused", "failed", "bad_request"].iter().find_map(|k| a.get(k).and_then(Json::as_str)).unwrap_or("no reason given").to_string()
    }

    fn ok(&mut self, line: &str) -> Result<Json, String> {
        let a = self.command(line)?;
        if a.get("ok").and_then(Json::as_bool) == Some(true) { Ok(a) } else { Err(Self::why(&a)) }
    }

    /// UPD-4 `part`: true when the medium holds it already (nothing to send).
    pub fn part(&mut self, name: &str, hash: &str, files: u64, dirs: u64, bytes: u64) -> Result<bool, String> {
        let a = self.ok(&format!("part {name} {hash} {files} {dirs} {bytes}"))?;
        Ok(a.get("present").and_then(Json::as_bool) == Some(true))
    }

    /// One file of the part being written: its line, then exactly `size` bytes of `data`.
    pub fn put_file(&mut self, path: &str, size: u64, data: &mut dyn Read) -> Result<(), String> {
        let a = self.ok(&format!("file {size} {path}"))?;
        if a.get("loading").is_none() {
            return Ok(());
        }
        let mut buf = vec![0u8; CHUNK];
        let mut left = size;
        while left > 0 {
            let want = (left as usize).min(CHUNK);
            let n = data.read(&mut buf[..want]).map_err(|e| format!("{path}: {e}"))?;
            if n == 0 {
                return Err(format!("{path}: the archive ends {left} bytes early"));
            }
            self.f.write_all(&buf[..n]).map_err(|e| format!("handing {path} over: {e}"))?;
            left -= n as u64;
        }
        let a = self.answer()?;
        if a.get("stored").is_some() { Ok(()) } else { Err(Self::why(&a)) }
    }

    pub fn seal(&mut self) -> Result<(), String> {
        self.ok("seal").map(|_| ())
    }

    pub fn use_part(&mut self, name: &str, hash: &str) -> Result<(), String> {
        self.ok(&format!("use {name} {hash}")).map(|_| ())
    }

    /// `begin`, both files, then `install unpin` (an update Georg starts ends a pin he set with "Zurück").
    pub fn install(&mut self, version: u32, kernel: &[u8], initrd: &[u8]) -> Result<Json, String> {
        let a = self.command(&format!("begin {version} {} {}", kernel.len(), initrd.len()))?;
        if a.get("ok").and_then(Json::as_bool) != Some(true) {
            return Ok(a);
        }
        for part in [kernel, initrd] {
            for c in part.chunks(CHUNK) {
                self.f.write_all(c).map_err(|e| format!("handing the bundle over: {e}"))?;
            }
        }
        let a = self.answer()?;
        if a.get("loaded").is_none() {
            return Err(format!("the kernel did not take the whole bundle: {}", a.to_string()));
        }
        self.command("install unpin")
    }
}
