use super::*;

const ALL: [Typeface; 4] = [Typeface::Bricolage, Typeface::Figtree, Typeface::JetBrainsMono, Typeface::Exo2];

fn ink(c: &Canvas) -> Vec<(i32, i32)> {
    (0..c.h).flat_map(|y| (0..c.w).map(move |x| (x, y))).filter(|&(x, y)| c.pixel(x, y) >> 24 > 128).collect()
}

#[test]
fn every_family_draws_german_text_and_the_boards_signs() {
    // The boards use − → ✓ ✗ ▾ ▸ ● ·; a lacking family takes them from another one or from the icons.
    let signs = "äöüÄÖÜß€…·°‹›„“−→✓✗▾▸●";
    for tf in ALL {
        for w in [Weight::Regular, Weight::Medium, Weight::SemiBold] {
            let f = Font::of(tf, 15.0, w, 1.0);
            for c in signs.chars() {
                assert!(f.draws(c), "{} {w:?} lacks {c}", tf.name());
            }
        }
    }
    assert!(Font::has('ä') && Font::has('✓'));
}

#[test]
fn a_missing_glyph_comes_from_the_next_family_with_its_metrics() {
    // fonts/README: Figtree lacks the arrow's neighbours; whatever it lacks, the box is the fallback face's advance.
    let f = Font::of(Typeface::Bricolage, 20.0, Weight::SemiBold, 1.0);
    let lacking: Vec<char> = "−→·€…„“".chars().filter(|&c| !face(f.own()).has_glyph(c)).collect();
    for c in lacking {
        let Src::Face(i) = f.src(c) else { panic!("{c} is no icon") };
        assert_ne!(i, f.own(), "{c} falls back");
        assert_eq!(f.boxes(&c.to_string())[0].1, face(i).metrics(c, 20.0).advance_width.round() as i32);
    }
    // A character no family has keeps the own face (its empty box), it does not vanish.
    assert!(!f.draws('\u{e000}'));
    assert!(f.width("\u{e000}") > 0);
}

#[test]
fn weights_take_the_nearest_shipped_file() {
    let w = |tf, w| FILES[file(tf, w)].1;
    assert_eq!(w(Typeface::Bricolage, Weight::Regular), 500, "Bricolage has 500 and 600 only");
    assert_eq!(w(Typeface::Bricolage, Weight::Bold), 600);
    assert_eq!(w(Typeface::Figtree, Weight::Light), 400);
    assert_eq!(w(Typeface::Figtree, Weight::Bold), 700);
    assert_eq!(w(Typeface::JetBrainsMono, Weight::Bold), 600);
    assert_eq!(w(Typeface::Exo2, Weight::Light), 300);
    for (i, &(tf, fw, _)) in FILES.iter().enumerate() {
        assert_eq!(FILES[file(tf, Weight::Regular)].0, tf);
        assert!(face(i).has_glyph('A'), "{} {fw} parses with glyphs", tf.name());
    }
}

#[test]
fn names_from_the_settings() {
    for tf in ALL {
        assert_eq!(Typeface::by_name(tf.name()), Some(tf));
    }
    assert_eq!(Typeface::by_name("jetbrains-mono"), Some(Typeface::JetBrainsMono));
    assert_eq!(Typeface::by_name("EXO2"), Some(Typeface::Exo2));
    assert_eq!(Typeface::by_name("Noto Sans Mono"), None);
    assert_eq!(Typeface::by_name(""), None);
}

#[test]
fn widths_follow_size_scale_spacing_and_family() {
    let a = Font::new(15.0, Weight::Regular, 1.0);
    let b = Font::new(15.0, Weight::Regular, 1.25);
    let (wa, wb) = (a.width("Arbeitsräume"), b.width("Arbeitsräume"));
    assert!(wa > 60 && wb > wa && (wb as f32 / wa as f32 - 1.25).abs() < 0.1, "{wa} {wb}");
    assert!(a.spaced(0.12).width("ARBEITSRÄUME") > a.width("ARBEITSRÄUME"));
    assert!(a.spaced(-0.02).width("ARBEITSRÄUME") < a.width("ARBEITSRÄUME"));
    assert!(b.height() > a.height());
    assert_eq!(a.width(""), 0);
    let exo = Font::of(Typeface::Exo2, 15.0, Weight::Regular, 1.0);
    assert_ne!(exo.width("Arbeitsräume"), wa, "the family changes the metrics");
    assert_eq!(a.typeface(), Typeface::Figtree);
}

#[test]
fn mono_has_one_advance() {
    let m = Font::of(Typeface::JetBrainsMono, 11.0, Weight::Medium, 4.0 / 3.0);
    let bx = m.boxes("iWm0.");
    assert!(bx.windows(2).all(|p| p[0].1 == p[1].1), "{bx:?}");
    assert_eq!(m.width("iiii"), m.width("WWWW"));
}

#[test]
fn display_sizes_at_the_board_scale() {
    // a-absicht: 44 board px; at 4/3 the line is about 59 px high (ascent+descent of Bricolage: ~1.2 em).
    let f = Font::of(Typeface::Bricolage, 44.0, Weight::SemiBold, 4.0 / 3.0);
    assert!((60..=85).contains(&f.height()), "{}", f.height());
    let l = Font::of(Typeface::JetBrainsMono, 11.0, Weight::Medium, 4.0 / 3.0);
    assert!((15..=24).contains(&l.height()), "{}", l.height());
}

#[test]
fn fitting_cuts_with_an_ellipsis() {
    let f = Font::new(13.0, Weight::Regular, 1.25);
    assert_eq!(f.fit("Grüße", 1000), "Grüße");
    let cut = f.fit("Langer_Fenstertitel_mit_viel_Text", 80);
    assert!(cut.ends_with('…') && f.width(&cut) <= 80, "{cut}");
    assert_eq!(f.fit("Grüße", 0), "");
}

#[test]
fn drawing_puts_ink_inside_the_line() {
    for tf in ALL {
        let f = Font::of(tf, 15.0, Weight::SemiBold, 1.25);
        let mut c = Canvas::new(160, 30);
        let w = c.text(&f, 2, 2, "Grüße €", [1.0; 4]);
        assert!(w > 40);
        assert!(ink(&c).len() > 80, "{}: {} pixels of ink", tf.name(), ink(&c).len());
    }
}

#[test]
fn icons_are_drawn_inside_their_box_and_the_line() {
    let f = Font::new(16.0, Weight::Regular, 4.0 / 3.0);
    for ch in ['✓', '✗', '▾', '▸', '●'] {
        let mut c = Canvas::new(60, 40);
        let w = c.text(&f, 10, 5, &ch.to_string(), [1.0; 4]);
        let px = ink(&c);
        assert!(px.len() > 15, "{ch}: {} pixels of ink", px.len());
        let (x0, x1) = (px.iter().map(|p| p.0).min().unwrap(), px.iter().map(|p| p.0).max().unwrap());
        let (y0, y1) = (px.iter().map(|p| p.1).min().unwrap(), px.iter().map(|p| p.1).max().unwrap());
        assert!(x0 >= 10 && x1 < 10 + w, "{ch}: ink {x0}..{x1} inside the advance {w}");
        assert!(y0 >= 5 && y1 < 5 + f.height(), "{ch}: ink {y0}..{y1} inside the line");
    }
    // The dot is round and filled: its middle is full ink.
    let mut c = Canvas::new(60, 40);
    c.text(&f, 10, 5, "●", [1.0; 4]);
    let px = ink(&c);
    let (mx, my) =
        (px.iter().map(|p| p.0).sum::<i32>() / px.len() as i32, px.iter().map(|p| p.1).sum::<i32>() / px.len() as i32);
    assert_eq!(c.pixel(mx, my) >> 24, 255);
}

#[test]
fn hostile_text_and_sizes_neither_panic_nor_disagree() {
    // Window titles and addresses come from programs: control characters, unassigned and astral code points, the
    // empty string, and sizes from 0 to absurd must measure finitely, and boxes must agree with the width.
    let texts = ["", "\u{0}\u{7}\u{1b}[31m", "\u{FFFF}\u{10FFFF}", "😀漢字ﷺ\u{202e}abc", "a\u{0301}\u{200d}b"];
    let sizes = [(0.0, 1.0), (15.0, 0.0), (15.0, f64::NAN), (15.0, f64::INFINITY), (1e9, 1e9), (15.0, 4.0 / 3.0)];
    for tf in ALL {
        for &(logical, scale) in &sizes {
            let f = Font::of(tf, logical, Weight::Medium, scale).spaced(0.08);
            assert!(f.px.is_finite() && (MIN_PX..=MAX_PX).contains(&f.px), "{} {logical} {scale}: {}", tf.name(), f.px);
            for s in texts {
                let w = f.width(s);
                let bx = f.boxes(s);
                assert_eq!(bx.len(), s.chars().count());
                if let Some(&(x, adv)) = bx.last() {
                    assert!((x + adv - w).abs() <= 2, "{} {s:?}: last box ends at {} of {w}", tf.name(), x + adv);
                } else {
                    assert_eq!(w, 0);
                }
                let fit = f.fit(s, 40);
                assert!(f.width(&fit) <= 40 || fit.is_empty(), "{s:?} fits as {fit:?}");
            }
        }
        let mut c = Canvas::new(64, 32);
        for s in texts {
            c.text(&Font::of(tf, 13.0, Weight::Regular, 1.25), 0, 0, s, [1.0; 4]);
        }
        // Drawn at the largest size into a small canvas: clipped, no panic.
        c.text(&Font::of(tf, 1e6, Weight::Regular, 1.0), -5, -5, "W✓", [1.0; 4]);
    }
}

#[test]
fn fitting_a_long_title_matches_the_exact_cut() {
    // `fit` estimates from one layout pass; the result must be the longest prefix whose cut fits, as measured exactly.
    let f = Font::of(Typeface::Figtree, 13.0, Weight::Regular, 1.25).spaced(0.02);
    let title = "Ein sehr langer Fenstertitel – mit Wörtern, Kürzeln (AV, To, Ty) und 漢字 am Ende ".repeat(8);
    for max in [0, 5, 17, 60, 133, 400, 900] {
        let cut = f.fit(&title, max);
        let chars: Vec<char> = title.chars().collect();
        let exact = (0..chars.len())
            .rev()
            .map(|n| chars[..n].iter().collect::<String>().trim_end().to_string() + "…")
            .find(|c| f.width(c) <= max)
            .unwrap_or_else(|| if f.width("…") <= max { "…".into() } else { String::new() });
        assert_eq!(cut, exact, "max {max}");
    }
}
