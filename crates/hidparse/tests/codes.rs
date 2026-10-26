//! src/codes.rs against /usr/include/linux/input-event-codes.h (where the header is installed).

#[test]
fn codes_match_the_linux_header() {
    let Ok(header) = std::fs::read_to_string("/usr/include/linux/input-event-codes.h") else {
        eprintln!("no input-event-codes.h: not checked");
        return;
    };
    let defines: std::collections::BTreeMap<&str, &str> = header
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            (w.next() == Some("#define")).then(|| (w.next().unwrap_or(""), w.next().unwrap_or("")))
        })
        .collect();
    let value = |name: &str| {
        let mut v = defines[name];
        loop {
            let n = if let Some(h) = v.strip_prefix("0x") {
                u16::from_str_radix(h, 16).ok()
            } else {
                v.parse().ok()
            };
            match n {
                Some(n) => return n,
                None => v = defines[v],
            }
        }
    };
    for &(name, v) in hidparse::codes::ALL {
        assert_eq!(value(name), v, "{name}");
    }
}
