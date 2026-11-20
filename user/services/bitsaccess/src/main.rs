//! The `bitsaccess` command.
#![forbid(unsafe_code)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::process::exit;

const USAGE: &str = "usage: bitsaccess COMMAND
  serve                         the service init starts: /dev/bitsos/access into /data/system/access
  query OP [key=value ...]      asks the running service (OP summary or events; keys since_s app kind max)
  settings                      the access.* values (registers the keys with settingsd when it lacks them)";

fn query(args: &[String]) -> Result<String, String> {
    let op = args.first().ok_or(USAGE)?;
    let mut pairs = Vec::new();
    for a in &args[1..] {
        let (k, v) = a.split_once('=').ok_or(USAGE)?;
        let j = match v.parse::<f64>() {
            Ok(n) if k != "app" && k != "kind" => mcpreg::Json::Num(n),
            _ => mcpreg::Json::str(v),
        };
        pairs.push((k, j));
    }
    let req = mcpreg::Json::obj(vec![("id", mcpreg::Json::Num(1.0)), ("op", mcpreg::Json::str(op)), ("args", mcpreg::Json::obj(pairs))]);
    let s = UnixStream::connect(bitsaccess::SOCKET).map_err(|e| format!("{}: {e}", bitsaccess::SOCKET))?;
    let mut w = s.try_clone().map_err(|e| e.to_string())?;
    writeln!(w, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => exit(bitsaccess::serve::serve()),
        Some("query") => match query(&args[1..]) {
            Ok(l) => print!("{l}"),
            Err(e) => {
                eprintln!("bitsaccess: {e}");
                exit(1)
            }
        },
        Some("settings") => {
            let (s, note) = bitsaccess::settings::load(&bitsaccess::settings::socket());
            println!("enabled={} keep_days={} max_mb={} ignore={:?} private_roots={:?}", s.enabled, s.keep_days, s.max_mb, s.ignore.join(";"), s.private_roots.join(";"));
            if let Some(n) = note {
                eprintln!("bitsaccess: settings: {n}");
            }
        }
        Some("--help" | "-h" | "help") => println!("{USAGE}"),
        _ => {
            eprintln!("{USAGE}");
            exit(2)
        }
    }
}
