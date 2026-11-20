//! The update's GitHub side end to end, with the real `fetch`: a fake GitHub over HTTPS (a test CA made with
//! openssl for this run, the API at https://localhost, the storage at https://127.0.0.1 on another port), the
//! release list, assets answered with a redirect to the storage host, which records whether the token arrived
//! (it must not: fetch drops Authorization on a redirect to another host) and the API which demands it.
//! Then: a wrong token (401), a release whose file was changed after signing, a release signed by another key,
//! and a medium that already holds the version. Lane NW's ports 24900-24999.

use bitsupdate::flow::{self, Ctx, Fail, Installed};
use bitsupdate::manifest::{Entry, Manifest, NOTES};
use bitsupdate::verify::{self, Why};
use bitsupdate::{sig, source};
use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "test_0123456789abcdefghij";

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("bitsupdate-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A CA and a certificate for localhost and 127.0.0.1 signed by it (openssl, this run only).
fn certs(d: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let run = |args: &[&str]| {
        let o = Command::new("openssl").args(args).current_dir(d).output().expect("openssl (the test CA is made with it)");
        assert!(o.status.success(), "openssl {args:?}: {}", String::from_utf8_lossy(&o.stderr));
    };
    run(&["req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-keyout", "ca.key", "-out", "ca.pem", "-days", "2", "-subj", "/CN=BitsOS update test CA"]);
    std::fs::write(d.join("ext.cnf"), "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n").unwrap();
    run(&["req", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes", "-keyout", "srv.key", "-out", "srv.csr", "-subj", "/CN=localhost"]);
    run(&["x509", "-req", "-in", "srv.csr", "-CA", "ca.pem", "-CAkey", "ca.key", "-CAcreateserial", "-out", "srv.pem", "-days", "2", "-extfile", "ext.cnf"]);
    (d.join("ca.pem"), d.join("srv.pem"), d.join("srv.key"))
}

fn tls(cert: &Path, key: &Path) -> Arc<ServerConfig> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert).unwrap().collect::<Result<_, _>>().unwrap();
    let key = PrivateKeyDer::from_pem_file(key).unwrap();
    let p = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(ServerConfig::builder_with_provider(p).with_safe_default_protocol_versions().unwrap().with_no_client_auth().with_single_cert(chain, key).unwrap())
}

fn listener() -> (TcpListener, u16) {
    for port in 24900..25000 {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", port)) {
            return (l, port);
        }
    }
    panic!("no free port in 24900-24999");
}

/// One request: its target and headers (lower-case names).
struct Req {
    target: String,
    headers: Vec<(String, String)>,
}

impl Req {
    fn header(&self, n: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str())
    }
}

/// Serves forever on `l`: `answer` gives (status line, headers, body) for each request.
fn serve(l: TcpListener, cfg: Arc<ServerConfig>, answer: Arc<dyn Fn(&Req) -> (String, String, Vec<u8>) + Send + Sync>) {
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (cfg, answer) = (cfg.clone(), answer.clone());
            std::thread::spawn(move || {
                let Ok(conn) = rustls::ServerConnection::new(cfg) else { return };
                let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                let mut t = rustls::StreamOwned::new(conn, s);
                let mut r = BufReader::new(&mut t);
                let mut first = String::new();
                if r.read_line(&mut first).is_err() {
                    return;
                }
                let mut headers = Vec::new();
                loop {
                    let mut l = String::new();
                    if r.read_line(&mut l).is_err() || l.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = l.trim_end().split_once(':') {
                        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
                    }
                }
                let target = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                let (status, extra, body) = answer(&Req { target, headers });
                let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n", body.len());
                let _ = t.write_all(head.as_bytes());
                let _ = t.write_all(&body);
                let _ = t.flush();
                t.conn.send_close_notify();
                let _ = t.flush();
                let mut rest = Vec::new();
                let _ = t.read_to_end(&mut rest);
            });
        }
    });
}

/// The fetch binary of this workspace (built here if a run of this test alone has not built it yet). It sits in the
/// profile folder above this test's binary: `target/debug/deps/` with the old layout, deeper below
/// `target/debug/build/<package>/<hash>/out/` with the newer cargo's build-dir layout (the gate's cargo).
fn fetch_exe() -> PathBuf {
    let me = std::env::current_exe().unwrap();
    let find = || me.ancestors().skip(1).map(|d| d.join("fetch")).find(|f| f.is_file());
    if let Some(exe) = find() {
        return exe;
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| String::from("cargo"));
    let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let st = Command::new(cargo).args(["build", "-q", "-p", "fetch"]).current_dir(&ws).status().unwrap();
    assert!(st.success(), "building fetch for the test");
    find().unwrap_or_else(|| panic!("no fetch binary above {}", me.display()))
}

struct Fake {
    files: Mutex<Vec<(u64, String, Vec<u8>)>>,
    storage_with_token: Mutex<u32>,
    storage_hits: Mutex<u32>,
    api_without_token: Mutex<u32>,
}

#[test]
fn check_download_and_every_refusal_against_a_fake_github() {
    let d = scratch("gh");
    let (ca, cert, key) = certs(&d);
    let cfg = tls(&cert, &key);
    let (api_l, api_port) = listener();
    let (sto_l, sto_port) = listener();
    let fake = Arc::new(Fake { files: Mutex::new(Vec::new()), storage_with_token: Mutex::new(0), storage_hits: Mutex::new(0), api_without_token: Mutex::new(0) });

    // The release: version 166, signed by the "desk" key of this test.
    let (desk, public) = sig::keygen().unwrap();
    let kernel: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
    let initrd: Vec<u8> = (0..200_000u32).map(|i| (i * 3) as u8).collect();
    let notes = "Was ist neu: das Aktualisieren.\n".as_bytes().to_vec();
    let m = Manifest {
        version: 166,
        date: "2026-11-16".into(),
        min_previous: None,
        files: vec![
            Entry { name: "KERNEL".into(), size: kernel.len() as u64, sha256: verify::sha256(&kernel) },
            Entry { name: "INITRD".into(), size: initrd.len() as u64, sha256: verify::sha256(&initrd) },
            Entry { name: NOTES.into(), size: notes.len() as u64, sha256: verify::sha256(&notes) },
        ],
        bins: vec![],
        trees: vec![],
    };
    let mtext = m.render().into_bytes();
    let msig = sig::sign(&desk, &mtext).unwrap().into_bytes();
    *fake.files.lock().unwrap() = vec![(1, "MANIFEST".into(), mtext), (2, "MANIFEST.SIG".into(), msig), (3, NOTES.into(), notes), (4, "KERNEL".into(), kernel.clone()), (5, "INITRD".into(), initrd.clone())];

    let f2 = fake.clone();
    serve(api_l, cfg.clone(), Arc::new(move |r: &Req| {
        if r.header("authorization") != Some(&format!("Bearer {TOKEN}")) {
            *f2.api_without_token.lock().unwrap() += 1;
            return ("401 Unauthorized".into(), String::new(), br#"{"message":"Bad credentials"}"#.to_vec());
        }
        let files = f2.files.lock().unwrap().clone();
        if r.target.starts_with("/repos/o/werkstatt/releases?") {
            assert_eq!(r.header("accept"), Some("application/vnd.github+json"));
            let assets: Vec<String> = files.iter().map(|(id, n, b)| format!(r#"{{"id":{id},"name":"{n}","size":{}}}"#, b.len())).collect();
            let body = format!(r#"[{{"tag_name":"version-166","draft":false,"published_at":"2026-11-16T09:00:00Z","assets":[{}]}},{{"tag_name":"version-165","draft":false,"assets":[]}}]"#, assets.join(","));
            return ("200 OK".into(), "Content-Type: application/json\r\n".into(), body.into_bytes());
        }
        if let Some(id) = r.target.strip_prefix("/repos/o/werkstatt/releases/assets/") {
            assert_eq!(r.header("accept"), Some("application/octet-stream"));
            // GitHub's answer: a redirect to a signed storage address on another host.
            return ("302 Found".into(), format!("Location: https://127.0.0.1:{sto_port}/blob/{id}?X-Amz-Signature=abc\r\n"), Vec::new());
        }
        ("404 Not Found".into(), String::new(), b"{}".to_vec())
    }));
    let f3 = fake.clone();
    serve(sto_l, cfg, Arc::new(move |r: &Req| {
        *f3.storage_hits.lock().unwrap() += 1;
        if r.header("authorization").is_some() {
            *f3.storage_with_token.lock().unwrap() += 1;
            // What S3 does with a second credential: refuse.
            return ("400 Bad Request".into(), String::new(), b"only one auth mechanism allowed".to_vec());
        }
        let id: u64 = r.target.trim_start_matches("/blob/").split('?').next().unwrap().parse().unwrap();
        let files = f3.files.lock().unwrap().clone();
        match files.iter().find(|f| f.0 == id) {
            Some((_, _, b)) => ("200 OK".into(), String::new(), b.clone()),
            None => ("404 Not Found".into(), String::new(), Vec::new()),
        }
    }));

    // The boot medium's UPDATE folder of this test, and a fetch that runs as we are.
    let dir = d.join("UPDATE");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SOURCE.TXT"), format!("api=https://localhost:{api_port}\nrepo=o/werkstatt\nca={}\n", ca.display())).unwrap();
    // The token's folder is settingsd's on the medium; its own folder here.
    let tdir = d.join("settings");
    let cx = Ctx { dir: dir.clone(), token_dir: tdir.clone(), work: d.join("work"), key: Some(public), fetch: fetch_exe() };
    let have = Installed { running: 165, newest: 165, ..Installed::default() };

    // No token yet.
    assert_eq!(flow::check(&cx, &have), Err(Fail::NoToken));
    // A wrong token: 401.
    std::fs::create_dir_all(&tdir).unwrap();
    std::fs::write(tdir.join(source::TOKEN), "test_wrong_wrong_wrong_wrong\n").unwrap();
    assert_eq!(flow::check(&cx, &have), Err(Fail::TokenWrong));
    source::store_token(&tdir, TOKEN).unwrap();
    assert!(!dir.join(source::TOKEN).exists(), "the token is not in the UPDATE folder");

    // The offer: the signed manifest and notes, checked.
    let o = flow::check(&cx, &have).unwrap().expect("version 166 is newer");
    assert_eq!((o.release.version, o.manifest.version, o.notes.as_str()), (166, 166, "Was ist neu: das Aktualisieren."));
    let mut steps = Vec::new();
    let b = flow::download(&cx, &have, &o, &mut |s| steps.push(s)).unwrap();
    assert_eq!((b.version, b.kernel == kernel, b.initrd == initrd), (166, true, true));
    assert_eq!(steps.len(), 5, "{steps:?}");
    assert!(*fake.storage_hits.lock().unwrap() >= 5, "every asset came from the storage host");
    assert_eq!(*fake.storage_with_token.lock().unwrap(), 0, "the token never reached the storage host");
    assert_eq!(*fake.api_without_token.lock().unwrap(), 1, "the API saw the token on every request but the wrong-token one");
    // UPD-4: the bundle keeps its folder (the parts' assets) until it is dropped.
    drop(b);
    assert!(std::fs::read_dir(d.join("work")).unwrap().next().is_none(), "the download folder is gone");

    // Already installed.
    let up = Installed { running: 166, newest: 166, ..Installed::default() };
    assert_eq!(flow::check(&cx, &up), Ok(None));

    // The file changed on the server after signing: refused before anything is handed on.
    let mut bad = kernel.clone();
    bad[1234] ^= 0x20;
    fake.files.lock().unwrap()[3].2 = bad;
    assert!(matches!(flow::download(&cx, &have, &o, &mut |_| {}), Err(Fail::Check(Why::File { ref name, .. })) if name == "KERNEL"));
    // A cut-short download (the server's file is shorter than signed).
    fake.files.lock().unwrap()[3].2 = kernel[..1000].to_vec();
    assert!(matches!(flow::download(&cx, &have, &o, &mut |_| {}), Err(Fail::Check(Why::File { .. }))));
    // Signed by someone else: the line Georg reads.
    let other = Ctx { key: Some(sig::keygen().unwrap().1), dir: dir.clone(), token_dir: tdir.clone(), work: d.join("work"), fetch: fetch_exe() };
    let f = flow::check(&other, &have).unwrap_err();
    assert_eq!(f.text(), "Nicht installiert: die Version ist nicht von deinem Desk unterschrieben.");
    let _ = std::fs::remove_dir_all(&d);
}
