//! JSON and the settings file format.

use settingsd::json::{Json, parse};
use settingsd::tomlish;
use settingsd::value::Value;

#[test]
fn json_round_trip_and_errors() {
    let t = r#"{"a":[1,-2,3.5,true,false,null],"s":"x\"\\\n\u00e4\ud83d\ude00","o":{}}"#;
    let j = parse(t).unwrap();
    assert_eq!(j.get("s").and_then(Json::as_str), Some("x\"\\\nä😀"));
    assert_eq!(parse(&j.to_text()).unwrap(), j);
    for bad in ["", "{", "[1,]", "{\"a\" 1}", "\"\u{1}\"", "01x", "1e999", "tru", "{\"a\":1}x", "\"\\q\""] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
    assert!(parse(&"[".repeat(65)).is_err());
    assert_eq!(parse("9223372036854775807").unwrap(), Json::Int(i64::MAX));
}

#[test]
fn toml_round_trip() {
    let e = vec![
        ("theme.font.ui".to_string(), Value::Text("Exo \"2\"\\ä".into())),
        ("theme.scale".to_string(), Value::Int(150)),
        ("x.on".to_string(), Value::Bool(true)),
    ];
    let text = tomlish::write("theme", &e);
    let p = tomlish::read(&text);
    assert!(p.bad_lines.is_empty());
    let mut got = p.entries.clone();
    got.sort();
    let mut want = e.clone();
    want.sort();
    assert_eq!(got, want);
    let p = tomlish::read("a.b = 1_000\n[t]\n'q.k' = 'lit'\n x = \"\\u00e4\"\nbad\n[unclosed\n");
    assert_eq!(
        p.entries,
        vec![
            ("a.b".to_string(), Value::Int(1000)),
            ("t.q.k".to_string(), Value::Text("lit".into())),
            ("t.x".to_string(), Value::Text("ä".into()))
        ]
    );
    assert_eq!(p.bad_lines.iter().map(|b| b.0).collect::<Vec<_>>(), vec![5, 6]);
}
