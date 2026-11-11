use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::desktop_entry::{self as de, parse};
use crate::icon_png;

/// A fresh scratch folder under the target dir (never outside the worktree).
fn scratch(name: &str) -> PathBuf {
    let target = option_env!("CARGO_TARGET_DIR").map_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target"), PathBuf::from);
    let d = target.join("pk-l1a-tests").join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn script(p: &Path) {
    use std::os::unix::fs::PermissionsExt;
    write(p, "#!/bin/sh\nexit 0\n");
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn desktop(name: &str, exec: &str, extra: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n{extra}")
}

#[test]
fn german_order_puts_umlauts_with_their_letter() {
    let mut v = vec!["Birne", "Ärger", "Apfel", "Opa", "Öl", "Obst", "zebra", "Straße", "Strasse2", "über"];
    v.sort_by(|a, b| order(a, b));
    assert_eq!(v, ["Apfel", "Ärger", "Birne", "Obst", "Öl", "Opa", "Straße", "Strasse2", "über", "zebra"]);
    assert_eq!(fold("Straße"), "strasse");
    assert_eq!(fold("ÄÖÜ"), "aou");
    assert_eq!(letter("äpfel"), 'Ä');
    assert_eq!(letter("ßtest"), 'ß');
    assert_eq!(letter(""), '?');
}

#[test]
fn desktop_entry_fields_and_german_names() {
    let t = "# c\n[Desktop Entry]\nType=Application\nName=Text Editor\nName[de]=Texteditor\nName[de_DE]=Texteditor DE\n\
             GenericName[de]=Textbearbeitung\nComment=Edit\nIcon=org.x.Editor\nExec=editor --new %U\nTryExec=editor\n\
             Terminal=false\nCategories=Utility;TextEditor;\nKeywords=text;edit;\nKeywords[de]=Text;Schreiben\\;Lesen;\n\
             StartupWMClass=org.x.Editor\nActions=new-window;\nOnlyShowIn=GNOME;BitsOS;\n\n[Desktop Action new-window]\nName=New\nExec=editor\n";
    let e = parse(t.as_bytes(), "/x.desktop").unwrap();
    assert_eq!(e.name, "Texteditor DE");
    assert_eq!(e.generic, "Textbearbeitung");
    assert_eq!(e.exec, ["editor", "--new"]);
    assert_eq!(e.keywords, ["Text", "Schreiben;Lesen"]);
    assert_eq!(e.categories, ["Utility", "TextEditor"]);
    assert_eq!(e.wm_class, "org.x.Editor");
    assert_eq!(e.actions, ["new-window"]);
    assert_eq!(e.try_exec, "editor");
    assert_eq!(e.not_shown(), None);
    let hide = |extra: &str| parse(desktop("A", "a", extra).as_bytes(), "").unwrap().not_shown();
    assert_eq!(hide("NoDisplay=true\n"), Some("NoDisplay=true"));
    assert_eq!(hide("Hidden=true\n"), Some("Hidden=true"));
    assert_eq!(hide("OnlyShowIn=KDE;\n"), Some("OnlyShowIn without BitsOS"));
    assert_eq!(hide("NotShowIn=BitsOS;\n"), Some("NotShowIn=BitsOS"));
    assert!(parse(b"[Desktop Entry]\nType=Link\nName=A\nURL=x\n", "").unwrap_err().contains("Type=Link"));
    assert!(parse(b"[Desktop Entry]\nType=Application\nExec=a\n", "").unwrap_err().contains("no Name"));
    // One sloppy line does not take the program out: a bad boolean is false, a bad key is skipped, with warnings.
    let lax = parse(desktop("A", "a", "Terminal=yes\nNoDisplay=1\nX_Bad Key=1\n[Desktop Action x]\nName[=y\n").as_bytes(), "").unwrap();
    assert!(!lax.terminal && !lax.no_display);
    assert_eq!(lax.warnings.len(), 4, "{:?}", lax.warnings);
}

#[test]
fn exec_quoting_and_field_codes_as_the_spec_says() {
    let ex = |s: &str| de::exec_args(&de::unescape(s), "ic", "Name X", "/a/b.desktop");
    assert_eq!(ex("foo %f").unwrap(), ["foo"]);
    assert_eq!(ex("\"/opt/my app/bin\" --x %U").unwrap(), ["/opt/my app/bin", "--x"]);
    // The value's own escapes first (\\ -> \), then the quoting: `\"` inside quotes is a quote.
    assert_eq!(ex(r#"sh -c "echo \\"hi\\" \\$HOME \\\\""#).unwrap(), ["sh", "-c", r#"echo "hi" $HOME \"#]);
    assert_eq!(ex("printf 100%%").unwrap(), ["printf", "100%"]);
    assert_eq!(ex("app %i").unwrap(), ["app", "--icon", "ic"]);
    assert_eq!(de::exec_args("app %i", "", "n", "").unwrap(), ["app"]);
    assert_eq!(ex("app --name=%c %k").unwrap(), ["app", "--name=Name X", "/a/b.desktop"]);
    assert_eq!(ex("app %d %D %n %N %v %m %F").unwrap(), ["app"]);
    assert_eq!(ex("app --file=%f").unwrap(), ["app", "--file="]);
    assert_eq!(ex("app \"\"").unwrap(), ["app", ""]);
    assert_eq!(ex("app \"100%%\"").unwrap(), ["app", "100%"]);
    assert_eq!(ex("  a\\s  b ").unwrap(), ["a", "b"]);
    assert!(ex("app %z").unwrap_err().contains("%z"));
    assert!(ex("app \"open").unwrap_err().contains("unterminated"));
    assert!(ex("app %").is_err());
    // Field codes inside quotes (forbidden by the spec, read as glib reads them).
    assert_eq!(ex("sh -c \"foo %U\"").unwrap(), ["sh", "-c", "foo "]);
    assert_eq!(ex("sh -c \"run %c %k %i\"").unwrap(), ["sh", "-c", "run Name X /a/b.desktop --icon ic"]);
    assert_eq!(ex("app \"%f\" \"%F\"x").unwrap(), ["app", "x"], "a quoted code alone leaves no argument");
    assert_eq!(ex("app \"printf %s\"").unwrap(), ["app", "printf %s"], "an unknown code inside quotes is kept");
}

#[test]
fn hostile_desktop_files_are_refused_without_a_panic() {
    let big = desktop("A", "a", &format!("Comment={}\n", "x".repeat(de::MAX_BYTES)));
    assert!(parse(big.as_bytes(), "").unwrap_err().contains("larger"), "64 KiB + 1");
    let mut exact = desktop("A", "a", "Comment=");
    exact.push_str(&"x".repeat(de::MAX_BYTES - exact.len() - 1));
    exact.push('\n');
    assert_eq!(exact.len(), de::MAX_BYTES);
    assert!(parse(exact.as_bytes(), "").is_ok(), "exactly 64 KiB is read");
    let mut bin = desktop("A", "a", "").into_bytes();
    bin.extend_from_slice(&[0xff, 0xfe, 0, 1, 0x80]);
    assert!(parse(&bin, "").unwrap_err().contains("UTF-8"));
    assert!(parse(format!("[Desktop Entry]\nName={}\n", "y".repeat(1 << 20)).as_bytes(), "").is_err(), "a line of 1 MiB");
    assert!(parse(desktop("A", "app \"open", "").as_bytes(), "").unwrap_err().contains("unterminated"));
    let mut groups = desktop("A", "a", "");
    for i in 0..10_000 {
        groups.push_str(&format!("[G{i}]\n"));
    }
    assert!(parse(groups.as_bytes(), "").is_err(), "10 000 groups");
    let mut lines = desktop("A", "a", "");
    lines.push_str(&"#\n".repeat(de::MAX_LINES));
    assert!(parse(lines.as_bytes(), "").unwrap_err().contains("lines"));
    for bad in ["Name=A\n[Desktop Entry]\n", "[Other]\nA=b\n", "[Desktop Entry\nType=Application\n", "[Desktop Entry]\nno equals\n",
        "[Desktop Entry]\n[Desktop Entry]\n", "[Desktop Entry]\nKey With Space=1\n", "", "\0\0\0"] {
        assert!(parse(bad.as_bytes(), "").is_err(), "{bad:?}");
    }
}

/// Every `.desktop` file of the build host, copied into a scratch folder: none may panic.
#[test]
fn corpus_of_the_build_hosts_desktop_files() {
    let src = Path::new("/usr/share/applications");
    let dir = scratch("apps-corpus");
    let mut n = 0;
    for e in std::fs::read_dir(src).expect("the build host has /usr/share/applications").flatten() {
        let p = e.path();
        if p.extension().is_some_and(|x| x == "desktop") && std::fs::copy(&p, dir.join(e.file_name())).is_ok() {
            n += 1;
        }
    }
    let (mut ok, mut refused, mut hidden) = (0, 0, 0);
    for p in super::files_with(&dir, "desktop") {
        let bytes = std::fs::read(&p).unwrap();
        match parse(&bytes, &p.to_string_lossy()) {
            Ok(e) if e.not_shown().is_some() => hidden += 1,
            Ok(_) => ok += 1,
            Err(e) => {
                refused += 1;
                println!("refused {}: {e}", p.file_name().unwrap().to_string_lossy());
            }
        }
    }
    println!("desktop corpus: {n} files, {ok} accepted, {refused} refused, {hidden} not shown (NoDisplay/Hidden/OnlyShowIn)");
    assert!(n > 0 && ok > 0 && hidden > 0, "{n} {ok} {hidden}");
    assert_eq!(ok + refused + hidden, n);
    let _ = std::fs::remove_dir_all(&dir);
}

fn png(w: u32, h: u32, colour: u8, depth: u8, rows: &[u8], extra: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let chunk = |out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&icon_png::crc32(&c).to_be_bytes());
    };
    let mut out = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
    let mut ih = Vec::new();
    ih.extend_from_slice(&w.to_be_bytes());
    ih.extend_from_slice(&h.to_be_bytes());
    ih.extend_from_slice(&[depth, colour, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ih);
    for (k, d) in extra {
        chunk(&mut out, k, d);
    }
    chunk(&mut out, b"IDAT", &miniz_oxide::deflate::compress_to_vec_zlib(rows, 6));
    chunk(&mut out, b"IEND", &[]);
    out
}

#[test]
fn png_colour_types_filters_and_refusals() {
    // 2x2 RGBA, the rows with filters Sub and Paeth.
    let rows = [1, 10, 20, 30, 255, 5, 5, 5, 0, 4, 1, 2, 3, 129, 0, 0, 0, 0];
    let img = icon_png::decode(&png(2, 2, 6, 8, &rows, &[])).unwrap();
    assert_eq!(img.px, [10, 20, 30, 255, 15, 25, 35, 255, 11, 22, 33, 128, 15, 25, 35, 128]);
    // Palette 2 bits with tRNS; grey 1 bit; grey+alpha; RGB 16 bits with a transparent colour.
    let pal = icon_png::decode(&png(3, 1, 3, 2, &[0, 0b0001_1000], &[(b"PLTE", vec![1, 2, 3, 4, 5, 6, 7, 8, 9]), (b"tRNS", vec![0, 99])])).unwrap();
    assert_eq!(pal.px, [1, 2, 3, 0, 4, 5, 6, 99, 7, 8, 9, 255]);
    let grey = icon_png::decode(&png(3, 1, 0, 1, &[0, 0b1010_0000], &[])).unwrap();
    assert_eq!(grey.px, [255, 255, 255, 255, 0, 0, 0, 255, 255, 255, 255, 255]);
    let ga = icon_png::decode(&png(1, 1, 4, 8, &[0, 7, 9], &[])).unwrap();
    assert_eq!(ga.px, [7, 7, 7, 9]);
    let rgb16 = icon_png::decode(&png(2, 1, 2, 16, &[0, 1, 2, 3, 4, 5, 6, 9, 9, 9, 9, 9, 9], &[(b"tRNS", vec![1, 2, 3, 4, 5, 6])])).unwrap();
    assert_eq!(rgb16.px, [1, 3, 5, 0, 9, 9, 9, 255]);
    // Refused with a reason.
    let good = png(2, 2, 6, 8, &rows, &[]);
    assert!(icon_png::decode(&good[..good.len() - 20]).is_err(), "truncated");
    let mut crc = good.clone();
    crc[40] ^= 1;
    assert!(icon_png::decode(&crc).unwrap_err().contains("CRC"));
    let mut inter = png(1, 1, 6, 8, &[0, 1, 2, 3, 4], &[]);
    inter[28] = 1;
    let c = icon_png::crc32(&inter[12..29]).to_be_bytes();
    inter[29..33].copy_from_slice(&c);
    assert!(icon_png::decode(&inter).unwrap_err().contains("Adam7"));
    assert!(icon_png::decode(&png(1025, 1, 6, 8, &[], &[])).unwrap_err().contains("larger"));
    assert!(icon_png::decode(&png(1, 1, 6, 8, &[9, 0, 0, 0, 0], &[])).unwrap_err().contains("filter"));
    assert!(icon_png::decode(&png(2, 1, 6, 8, &[0, 1, 2, 3, 4], &[])).is_err(), "too little data");
    assert!(icon_png::decode(&png(1, 1, 3, 8, &[0, 0], &[])).unwrap_err().contains("PLTE"));
    assert!(icon_png::decode(b"GIF89a").is_err());
    // Hostile sizes: more image data than the header allows (the inflate is bounded), an unknown critical chunk.
    let mut long = rows.to_vec();
    long.extend_from_slice(&[0; 4096]);
    assert!(icon_png::decode(&png(2, 2, 6, 8, &long, &[])).is_err(), "more data than 2x2");
    assert!(icon_png::decode(&png(2, 2, 6, 8, &rows, &[(b"ABCD", vec![1])])).unwrap_err().contains("critical"));
    assert!(icon_png::decode(&png(2, 2, 6, 8, &rows, &[(b"abCD", vec![1])])).is_ok(), "an ancillary chunk is skipped");
}

#[test]
fn icons_scale_to_the_tile_premultiplied() {
    // 2x2: white opaque, black opaque, transparent, half-transparent red -> 1x1.
    let img = icon_png::Rgba { w: 2, h: 2, px: vec![255, 255, 255, 255, 0, 0, 0, 255, 9, 9, 9, 0, 255, 0, 0, 128] };
    let t = icon_png::to_tile(&img, 1);
    let px = t.px[0];
    assert_eq!((px >> 24, (px >> 16) & 255, (px >> 8) & 255, px & 255), (160, 96, 64, 64));
    let wide = icon_png::Rgba { w: 4, h: 2, px: vec![255; 32] };
    let t = icon_png::to_tile(&wide, 4);
    assert_eq!((t.w, t.h), (4, 4));
    assert_eq!(t.px[0], 0, "letterboxed above");
    assert_eq!(t.px[4], 0xffff_ffff);
    let up = icon_png::to_tile(&icon_png::Rgba { w: 1, h: 1, px: vec![0, 0, 255, 255] }, 128);
    assert!(up.px.iter().all(|&p| p == 0xff00_00ff) && up.px.len() == 128 * 128);
}

/// Every PNG of the build host's hicolor 128x128 against Python's zlib reference (host/tests/wy/png_ref.py).
#[test]
fn png_corpus_matches_the_python_reference() {
    let dir = scratch("png-corpus");
    for e in std::fs::read_dir("/usr/share/icons/hicolor/128x128/apps").expect("the build host has hicolor 128x128").flatten() {
        if e.path().extension().is_some_and(|x| x == "png") {
            let _ = std::fs::copy(e.path(), dir.join(e.file_name()));
        }
    }
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../host/tests/wy/png_ref.py");
    let out = std::process::Command::new("python3").arg("-I").arg(&script).arg(&dir).output().expect("python3 runs the reference");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let copied = super::files_with(&dir, "png").len();
    let (mut same, mut not_counted) = (0, 0);
    for l in String::from_utf8(out.stdout).unwrap().lines() {
        let f: Vec<&str> = l.split('\t').collect();
        let bytes = std::fs::read(dir.join(f[0])).unwrap();
        let mine = icon_png::decode(&bytes);
        if f[1] == "-" {
            not_counted += 1;
            assert!(mine.is_err(), "{}: the reference refuses it ({}), the decoder read it", f[0], f[2]);
            continue;
        }
        let img = mine.unwrap_or_else(|e| panic!("{}: {e}", f[0]));
        assert_eq!((img.w.to_string(), img.h.to_string(), checksum(&img.px).to_string()), (f[1].into(), f[2].into(), f[3].into()), "{}", f[0]);
        same += 1;
    }
    println!("png corpus: {copied} files, {same} equal to the reference, {not_counted} not read by the reference (not counted)");
    assert!(same > 0);
    assert_eq!(same + not_counted, copied, "every file is either compared or refused by both");
    let _ = std::fs::remove_dir_all(&dir);
}

/// FNV-1a 64 over the RGBA bytes (png_ref.py computes the same).
fn checksum(px: &[u8]) -> u64 {
    px.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3))
}

fn exe(p: &Path) -> bool {
    crate::spawn::executable(p)
}

/// A fake stick bin with two launchers (one without its launcher file), a fake /apps with two programs.
fn fake(root: &Path) -> Roots {
    let (bin, apps) = (root.join("bin"), root.join("apps"));
    script(&bin.join("writerl"));
    write(&bin.join("writer.desktop"), &desktop("Writer", "writerl writer", "Icon=writer\nStartupWMClass=libreoffice-writer\n"));
    write(&bin.join("ghost.desktop"), &desktop("Ghost", "ghost", ""));
    write(&bin.join("terminal.desktop"), &desktop("Stick-Terminal", "writerl", ""));
    write(&bin.join("broken.desktop"), "\u{0}binary");
    // TryExec: one that names nothing is left out, one that is found stays.
    script(&bin.join("tryl"));
    write(&bin.join("tryno.desktop"), &desktop("TryNo", "tryl", "TryExec=does-not-exist\n"));
    write(&bin.join("tryyes.desktop"), &desktop("TryYes", "tryl", "TryExec=tryl\n"));
    // The icon is a symlink loop: refused by metadata, the fallback shows.
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink("writer.png", bin.join("writer.png")).unwrap();
    // Package "jq": a manifest program only; package "foot": a .desktop file and hicolor icons in its root tree.
    script(&apps.join("jq/1.0/bin/jq"));
    write(&apps.join("jq/1.0/manifest.toml"), "format = 1\nname = \"jq\"\nprogram = \"bin/jq\"\n[[file]]\npath = \"x\"\n");
    std::os::unix::fs::symlink("1.0", apps.join("jq/current")).unwrap();
    let r = apps.join("foot/2/root");
    script(&r.join("usr/bin/foot"));
    write(&r.join("usr/share/applications/terminal.desktop"), &desktop("Fuß-Terminal", "foot %U", "Icon=foot\nKeywords=shell;\n"));
    write(&r.join("usr/share/applications/hidden.desktop"), &desktop("Server", "foot --server", "NoDisplay=true\n"));
    write(&r.join("usr/share/applications/pkgtry.desktop"), &desktop("PkgTry", "foot", "TryExec=nothere\n"));
    let png = std::fs::read(std::fs::read_dir("/usr/share/icons/hicolor/128x128/apps").unwrap().flatten().map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "png") && std::fs::read(p).ok().and_then(|b| icon_png::decode(&b).ok()).is_some()).unwrap()).unwrap();
    for s in ["48x48", "256x256", "512x512"] {
        std::fs::create_dir_all(r.join(format!("usr/share/icons/hicolor/{s}/apps"))).unwrap();
        std::fs::write(r.join(format!("usr/share/icons/hicolor/{s}/apps/foot.png")), &png).unwrap();
    }
    std::os::unix::fs::symlink("2", apps.join("foot/current")).unwrap();
    Roots { bin, apps, path: String::new() }
}

#[test]
fn sources_duplicates_icons_and_search() {
    let root = scratch("sources");
    let roots = fake(&root);
    let mut s = scan(&roots, &exe);
    let ids: Vec<(&str, String)> = s.index.entries.iter().map(|e| (e.id.as_str(), e.source.word())).collect();
    assert_eq!(ids, [("einstellungen", "builtin".into()), ("terminal", "package:foot".into()), ("jq", "package:jq".into()),
        ("tryyes", "stick".into()), ("writer", "stick".into())], "A-Z by name: Einstellungen, Fuß-Terminal, Jq, TryYes, Writer; {:?}", s.notes);
    let notes = s.notes.join("\n");
    assert!(notes.contains("terminal from package:foot wins over stick") && notes.contains("wins over builtin"), "{notes}");
    assert!(notes.contains("ghost") && notes.contains("broken.desktop: refused") && notes.contains("NoDisplay"), "{notes}");
    assert!(notes.contains("tryno.desktop: left out: TryExec does-not-exist not found"), "{notes}");
    assert!(notes.contains("pkgtry.desktop: left out: TryExec nothere not in the package"), "{notes}");
    let foot = &s.index.entries[1];
    assert_eq!(foot.exec, [r_path(&roots, "foot/current/root/usr/bin/foot")]);
    assert_eq!(foot.address, "programm/terminal");
    assert!(matches!(&foot.icon, Icon::Pending(p) if p.ends_with("256x256/apps/foot.png")), "the smallest size >= 128: {:?}", foot.icon);
    let writer = s.index.by_address("programm/writer").unwrap();
    assert_eq!(writer.exec, ["/bin/sh".to_string(), roots.bin.join("writerl").to_string_lossy().into_owned(), "writer".into()]);
    assert!(writer.owns("libreoffice-writer") && !writer.owns("foot"));
    assert_eq!(s.index.by_address("writer").map(|e| e.id.as_str()), Some("writer"));
    assert_eq!(s.index.by_address("einstellungen").map(|e| e.id.as_str()), Some("einstellungen"));
    assert!(s.index.by_address("einstellungen/aussehen").is_none());
    let jq = s.index.by_address("programm/jq").unwrap();
    assert_eq!(jq.exec, [r_path(&roots, "jq/current/bin/jq")]);
    assert_eq!(jq.icon_state(), "fallback:J (no .desktop file)");
    let mut cache = IconCache::default();
    decode_icons(&mut s.index, &mut cache, &mut s.notes);
    assert!(matches!(&s.index.entries[1].icon, Icon::Image(_, a) if a.w == TILE && a.h == TILE));
    let w = s.index.by_address("programm/writer").unwrap();
    assert!(w.icon_state().starts_with("fallback:W ("), "the symlink loop: {}", w.icon_state());
    // Search: name word prefix before inside, GenericName and keywords after, ties A-Z.
    let names = |q: &str| s.index.matches(q).iter().map(|e| e.id.clone()).collect::<Vec<_>>();
    assert_eq!(names("ter"), ["terminal", "writer"], "a word start before inside a word");
    assert_eq!(names("fuss"), ["terminal"]);
    assert_eq!(names("SHELL"), ["terminal"]);
    assert_eq!(names("in"), ["einstellungen", "terminal"]);
    assert_eq!(names("writerl"), ["writer"], "the launcher's file name");
    assert_eq!(names("").len(), 5);
    assert!(names("zzz").is_empty());
    let lines = Index::lines(&s.index.matches(""));
    assert!(lines.lines().all(|l| l.split('\t').count() == 5), "{lines}");
    // A removed program's icon leaves the cache.
    assert_eq!(cache.map.len(), 1, "foot's PNG (writer's link loop fails before the cache)");
    cache.keep_used(&s.index);
    assert_eq!(cache.map.len(), 1, "foot's icon is still used");
    s.index.entries.retain(|e| e.id != "terminal");
    cache.keep_used(&s.index);
    assert_eq!(cache.map.len(), 0, "foot's program is gone, so is its icon");
    let _ = std::fs::remove_dir_all(&root);
}

/// `Hidden=true` deletes the id (spec): a lower source's entry of that id goes as well; Terminal=true runs in foot.
#[test]
fn hidden_masks_lower_sources_and_terminal_programs_run_in_foot() {
    let root = scratch("hidden");
    let (bin, apps) = (root.join("bin"), root.join("apps"));
    script(&bin.join("edl"));
    write(&bin.join("ed.desktop"), &desktop("Ed", "edl", "Terminal=true\n"));
    write(&bin.join("vi.desktop"), &desktop("Vi", "edl", "Terminal=true\n"));
    write(&apps.join("p/1/usr/share/applications/vi.desktop"), &desktop("Vi", "vi", "Hidden=true\n"));
    std::os::unix::fs::symlink("1", apps.join("p/current")).unwrap();
    write(&bin.join("einstellungen.desktop"), &desktop("X", "edl", "Hidden=true\n"));
    let s = scan(&Roots { bin: bin.clone(), apps, path: String::new() }, &exe);
    let ids: Vec<&str> = s.index.entries.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["ed", "terminal"], "{:?}", s.notes);
    let notes = s.notes.join("\n");
    assert!(notes.contains("vi from stick left out: Hidden=true in package:p"), "{notes}");
    assert!(notes.contains("einstellungen from builtin left out: Hidden=true in stick"), "{notes}");
    let ed = &s.index.entries[0];
    let mut want: Vec<String> = crate::terminal::TERMINAL.iter().map(|s| s.to_string()).collect();
    want.extend(["--app-id=ed".to_string(), "/bin/sh".to_string(), bin.join("edl").to_string_lossy().into_owned()]);
    assert_eq!(ed.argv(), want);
    assert!(ed.owns("ed") && !ed.owns("foot"), "its own foot window is ed's, another terminal is not");
    assert_eq!(s.index.entries[1].argv(), s.index.entries[1].exec, "a program that is no terminal program starts as it is");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn icon_names_with_an_extension_and_xpm() {
    let root = scratch("iconext");
    write(&root.join("pix/app.png"), "x");
    write(&root.join("pix/old.xpm"), "x");
    let pix = [root.join("pix")];
    assert_eq!(find_icon("app.png", None, &[], &pix), Ok(root.join("pix/app.png")));
    assert_eq!(find_icon("old.xpm", None, &[], &pix), Err("XPM only (not read)".into()));
    assert_eq!(find_icon("/x/y.xpm", None, &[], &pix), Err("XPM only (not read)".into()));
    let _ = std::fs::remove_dir_all(&root);
}

fn r_path(roots: &Roots, rel: &str) -> String {
    roots.apps.join(rel).to_string_lossy().into_owned()
}

/// The watch: a program added to the fake /apps shows up, a removed one goes, without a restart.
#[test]
fn programs_added_and_removed_while_watched() {
    use smithay::reexports::calloop::EventLoop;
    let root = scratch("watch");
    let roots = fake(&root);
    let mut ev: EventLoop<'static, Option<Arc<Index>>> = EventLoop::try_new().unwrap();
    start(&ev.handle(), roots.clone(), |d: &mut Option<Arc<Index>>, ix| *d = Some(ix)).unwrap();
    let mut data: Option<Arc<Index>> = None;
    let mut until = |data: &mut Option<Arc<Index>>, what: &str, ok: &dyn Fn(&Index) -> bool| {
        let end = Instant::now() + Duration::from_secs(10);
        while !data.as_ref().is_some_and(|i| ok(i)) {
            assert!(Instant::now() < end, "no index with {what} in 10 s: {:?}", data.as_ref().map(|i| i.entries.iter().map(|e| e.id.clone()).collect::<Vec<_>>()));
            ev.dispatch(Duration::from_millis(50), data).unwrap();
        }
    };
    until(&mut data, "the five entries", &|i| i.entries.len() == 5);
    script(&roots.apps.join("htop/3/bin/htop"));
    write(&roots.apps.join("htop/3/manifest.toml"), "program = \"bin/htop\"\n");
    std::os::unix::fs::symlink("3", roots.apps.join("htop/current")).unwrap();
    until(&mut data, "htop added", &|i| i.by_address("programm/htop").is_some());
    std::fs::remove_file(roots.apps.join("jq/current")).unwrap();
    until(&mut data, "jq removed", &|i| i.by_address("programm/jq").is_none());
    // A program folder deleted and made again is watched again (the folders go to the main loop after every scan).
    std::fs::remove_dir_all(roots.apps.join("htop")).unwrap();
    until(&mut data, "htop removed", &|i| i.by_address("programm/htop").is_none());
    std::fs::create_dir_all(roots.apps.join("htop")).unwrap();
    script(&roots.apps.join("htop/4/bin/htop"));
    write(&roots.apps.join("htop/4/manifest.toml"), "program = \"bin/htop\"\n");
    std::os::unix::fs::symlink("4", roots.apps.join("htop/current")).unwrap();
    until(&mut data, "htop back through its new folder's watch", &|i| i.by_address("programm/htop").is_some());
    std::fs::remove_file(roots.bin.join("writerl")).unwrap();
    until(&mut data, "writer's launcher gone", &|i| i.by_address("programm/writer").is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn program_name_sees_through_wrappers() {
    let mut e = Index::builtin().entries.into_iter().find(|e| e.id == "terminal").unwrap();
    let name = |e: &mut Entry, exec: &[&str]| {
        e.exec = exec.iter().map(|s| s.to_string()).collect();
        e.program_name().to_string()
    };
    assert_eq!(name(&mut e, &["env", "GDK_BACKEND=wayland", "-u", "X", "/usr/bin/gimp", "%U"]), "gimp");
    assert_eq!(name(&mut e, &["/usr/bin/flatpak", "run", "--branch=stable", "org.gnome.Maps"]), "org.gnome.Maps");
    assert_eq!(name(&mut e, &["/bin/sh", "/stick/brave"]), "brave");
    assert_eq!(name(&mut e, &["sh", "-c", "exec x"]), "", "a shell line says nothing about its program");
    for generic in [&["java", "-jar", "a.jar"][..], &["python3", "x.py"], &["electron25", "app"], &["env", "node", "a.js"], &[]] {
        assert_eq!(name(&mut e, generic), "", "{generic:?}");
    }
    e.address = "programm/tool".into();
    e.wm_class.clear();
    e.exec = vec!["java".into(), "-jar".into(), "tool.jar".into()];
    assert!(!e.owns("java"), "a minimised window of another java program is not this one's");
    e.exec = vec!["env".into(), "A=1".into(), "gimp".into()];
    assert!(e.owns("gimp") && e.owns("tool"));
}
