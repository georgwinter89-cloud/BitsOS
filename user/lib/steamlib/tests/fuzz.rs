//! GM-6s: 10 000 mutations of real-shaped files (fixed seed, no crate): the parser, the manifest reader and the
//! libraryfolders.vdf reader never panic.

use std::cell::RefCell;
use std::io;
use std::path::{Path, PathBuf};
use steamlib::Fs;

/// One libraryfolders.vdf in memory: the mutated bytes go through the folder reader too.
struct OneFile(RefCell<Vec<u8>>);
impl Fs for OneFile {
    fn read(&self, _: &Path, max: u64) -> io::Result<Vec<u8>> {
        let b = self.0.borrow();
        if b.len() as u64 > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "too long"));
        }
        Ok(b.clone())
    }
    fn list(&self, _: &Path, _: &dyn Fn(&str) -> bool, _: usize) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

const SEED: &str = "\"AppState\"\n{\n\t\"appid\"\t\"440\"\n\t\"name\"\t\"Spiel \\\"X\\\" C:\\\\\"\n\t\"StateFlags\"\t\"4\"\n\
                    \t\"SizeOnDisk\"\t\"99\" // c\n\t\"UserConfig\"\n\t{\n\t\t\"platform_override_source\"\t\"windows\"\n\t}\n}\n\
                    \"libraryfolders\" { \"0\" { \"path\" \"/a\" } \"1\" \"/b\" } [$X]\n";
const FOLDERS: &str = "\"libraryfolders\"\n{\n\t\"contentstatsid\"\t\"-1\"\n\t\"0\"\n\t{\n\t\t\"path\"\t\"/data/home/1000/.local/share/Steam\"\n\
                       \t\t\"apps\"\n\t\t{\n\t\t\t\"32360\"\t\"1\"\n\t\t}\n\t}\n\t\"1\"\t\"D:\\\\Spiele\\\\Steam\"\n\
                       \t\"2\" { \"path\" \"rel\" }\n}\n";
const ALPHABET: &[u8] = b"\"{}\\/\n\t []$x0\xff\xc3";

#[test]
fn ten_thousand_mutations_never_panic() {
    let mut r = Rng(0x6d5_6a1b_2c3d_4e5f);
    let mut parsed = 0;
    let mut folders = 0;
    let fs = OneFile(RefCell::new(Vec::new()));
    for i in 0..10_000 {
        let mut b = if i % 2 == 0 { SEED } else { FOLDERS }.as_bytes().to_vec();
        for _ in 0..1 + r.below(8) {
            let at = r.below(b.len() + 1);
            match r.below(5) {
                0 if at < b.len() => b[at] = ALPHABET[r.below(ALPHABET.len())],
                1 => b.insert(at, ALPHABET[r.below(ALPHABET.len())]),
                2 if at < b.len() => {
                    b.remove(at);
                }
                3 => b.truncate(at),
                _ => {
                    let n = r.below(100);
                    let what = if r.below(2) == 0 { "k {" } else { "}" };
                    for _ in 0..n {
                        b.splice(at.min(b.len())..at.min(b.len()), what.bytes());
                    }
                }
            }
        }
        if steamlib::kv::parse(&b).is_ok() {
            parsed += 1;
        }
        let _ = steamlib::parse_manifest(&b, 1, Path::new("/x"));
        *fs.0.borrow_mut() = b;
        let found: Vec<PathBuf> = steamlib::library_folders_with(&fs, Path::new("/r"), &mut Vec::new());
        assert_eq!(found[0], Path::new("/r"), "the root always comes first");
        folders += found.len() - 1;
    }
    assert!(folders > 0, "some mutated libraryfolders.vdf still name a library");
    assert!(parsed > 100 && parsed < 10_000, "the mutations reach both sides: {parsed} parsed");
}
