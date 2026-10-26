//! The `bitsjournal` command.
#![forbid(unsafe_code)]

use bitsjournal::{DIR, query, serve};
use mcpreg::Json;
use std::path::Path;
use std::process::exit;

const USAGE: &str = "usage: bitsjournal COMMAND
  serve [--once]            the service init starts: keeps /dev/bitsos/events in /data/system/journal
  query [key=value ...]     events (or kind=metrics) from the journal, keys as the MCP tool journal takes them:
                            since=/until= (unix seconds), level= subsystem= name= fields='a=1 b>2' source= boot=
                            order=newest max= agg=
  segments                  the journal's files, oldest first";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("serve") => exit(serve::serve(args.get(1).map(String::as_str) == Some("--once"))),
        Some("query") => {
            let mut pairs: Vec<(String, Json)> = Vec::new();
            for a in &args[1..] {
                let Some((k, v)) = a.split_once('=') else {
                    eprintln!("{USAGE}");
                    exit(2)
                };
                let j = match v.parse::<f64>() {
                    Ok(n) if k != "boot" && k != "fields" && k != "name" => Json::Num(n),
                    _ => Json::str(v),
                };
                pairs.push((k.to_string(), j));
            }
            let a = Json::Obj(pairs);
            let q = query::Query::from_json(&a, bitsjournal::now_ms()).unwrap_or_else(|e| {
                eprintln!("bitsjournal: {e}");
                exit(2)
            });
            let off = std::fs::read_to_string(Path::new(DIR).join("state")).ok().and_then(|t| serve::State::parse(&t)).map_or(60, |s| s.off_min);
            let out = match q.kind {
                query::Kind::Events => query::events(&q, Path::new(DIR), &[], off),
                query::Kind::Metrics => query::metrics(&q, Path::new(DIR), &[], off),
            };
            println!("{out}");
        }
        Some("segments") => match bitsjournal::store::segments(Path::new(DIR)) {
            Ok(v) => {
                for s in &v {
                    println!("{}  {}  {} bytes", s.path.display(), bitsjournal::local_time(s.start_s * 1000, 0), s.bytes);
                }
                println!("{} segments, {} bytes", v.len(), v.iter().map(|s| s.bytes).sum::<u64>());
            }
            Err(e) => {
                eprintln!("bitsjournal: {DIR}: {e}");
                exit(1)
            }
        },
        Some("--help" | "-h" | "help") => println!("{USAGE}"),
        _ => {
            eprintln!("{USAGE}");
            exit(2)
        }
    }
}
