use super::tokens::{Palette, ShadowLayer, spec};
use super::*;

fn h(s: &str) -> Rgba {
    hex(s).unwrap()
}

fn near(a: Rgba, b: Rgba, tol: f32) -> bool {
    (0..4).all(|i| (a[i] - b[i]).abs() <= tol)
}

fn in_mode(mode: &str) -> Values {
    let mut v = Values::default();
    v.set("theme.mode", mode);
    v
}

/// Every colour field with its key below `theme.<mode>.` (the accent tones only while no user accent is set).
fn fields(t: &Theme) -> Vec<(&'static str, Rgba)> {
    vec![
        ("bg", t.bg),
        ("surface", t.panel),
        ("surface2", t.panel2),
        ("paper", t.paper),
        ("border", t.line),
        ("text", t.text),
        ("text_dim", t.muted),
        ("line_strong", t.line_strong),
        ("accent", t.accent),
        ("accent_soft", t.accent_soft),
        ("on_accent", t.on_accent),
        ("claude", t.claude),
        ("warnung", t.danger),
        ("terminal", t.terminal),
        ("on_terminal", t.on_terminal),
        ("kontext_1", t.kontext[0]),
        ("kontext_2", t.kontext[1]),
        ("kontext_3", t.kontext[2]),
        ("on_kontext", t.on_kontext),
    ]
}

#[test]
fn defaults_are_the_design_v2_tokens() {
    for (p, mode) in [(&DARK, "dark"), (&LIGHT, "light")] {
        let t = Theme::build(&in_mode(mode));
        assert_eq!(t.dark, mode == "dark");
        let got = fields(&t);
        for ((k, d), (fk, c)) in p.keys().iter().zip(&got) {
            assert_eq!(k, fk);
            assert_eq!(*c, h(d), "{mode} {k}");
        }
        assert_eq!(t.chip, t.accent_soft, "selected things sit on accent-soft");
        assert_eq!(t.warm, t.kontext[1], "waiting is the amber");
    }
    let t = Theme::default();
    assert!(t.dark, "Absicht dunkel is the default");
    assert_eq!(t.accent, h("#7fcfa0"));
    assert!((t.glow[3] - 0.18).abs() < 1e-6);
    assert_eq!(t.scale, 1.25);
    // DS-3: the Bühne (default) spaces its tiles as the boards; the classic preset keeps the first chrome's metrics.
    assert_eq!((t.gap, t.radius, t.header_height), (26, 30, 38), "the Bühne's blocks");
    let mut cv = crate::settings::Values::default();
    cv.set("theme.layout.preset", "classic");
    let c = Theme::build(&cv);
    assert_eq!((c.gap, c.radius, c.header_height), (12, 12, 38), "the first chrome keeps its metrics");
    assert_eq!(c.layout.preset, Preset::Classic);
    assert_eq!(t.space, [4, 8, 12, 16, 24, 32, 48, 64]);
    assert_eq!(t.radius_card, 28);
    assert_eq!(t.board, 4.0 / 3.0, "the boards' 1920x1080 onto 2560x1440");
    assert_eq!(t.fonts, Families::default());
    assert_eq!(
        (t.font_display.as_str(), t.font_ui.as_str(), t.font_mono.as_str()),
        ("Bricolage Grotesque", "Figtree", "JetBrains Mono")
    );
    assert!(t.font_notes().is_empty());
}

#[test]
fn every_palette_key_is_read_and_a_stored_old_value_keeps_working() {
    for (p, mode) in [(&DARK, "dark"), (&LIGHT, "light")] {
        let mut v = in_mode(mode);
        for (n, (k, _)) in p.keys().iter().enumerate() {
            v.set(&format!("theme.{mode}.{k}"), &format!("#0102{:02x}", n + 1));
        }
        let t = Theme::build(&v);
        for (n, (k, c)) in fields(&t).into_iter().enumerate() {
            assert_eq!(c, h(&format!("#0102{:02x}", n + 1)), "{mode} {k}");
        }
    }
    // A palette value stored before design v2 (the WY-5 keys kept their names) still wins over the new default.
    let mut v = Values::default();
    v.set("theme.dark.bg", "#0b0f14");
    v.set("theme.dark.text_dim", "#93a4b8");
    let t = Theme::build(&v);
    assert_eq!((t.bg, t.muted), (h("#0b0f14"), h("#93a4b8")));
    assert_eq!(t.veil, alpha(h("#0b0f14"), 0.8));
}

#[test]
fn the_users_accent_overrides_both_modes_and_the_tones_follow() {
    for mode in ["dark", "light"] {
        let mut v = in_mode(mode);
        v.set("theme.accent", "#ffb020");
        let t = Theme::build(&v);
        assert_eq!(t.accent, h("#ffb020"), "{mode}");
        assert_eq!(t.on_accent, h(DARK.on_accent), "{mode}: dark text on a bright accent");
        assert_ne!(t.accent_soft, Theme::build(&in_mode(mode)).accent_soft, "{mode}: soft follows");
        assert_eq!(t.chip, t.accent_soft);
        assert!(t.glow[0] > t.glow[2], "{mode}: the glow is amber");
        // The cyan of the first look is one choice among others (the old default stored by a user keeps working).
        v.set("theme.accent", "#00e5ff");
        assert_eq!(Theme::build(&v).accent, h("#00e5ff"));
        v.set("theme.accent", "#123456");
        assert_eq!(Theme::build(&v).on_accent, h(LIGHT.on_accent), "{mode}: light text on a deep accent");
    }
    // settingsd's default and a malformed value mean the mode's own accent (moss dark, deep green light).
    let mut v = in_mode("light");
    for x in [ACCENT, "cyan", ""] {
        v.set("theme.accent", x);
        let t = Theme::build(&v);
        assert_eq!((t.accent, t.accent_soft), (h(LIGHT.accent), h(LIGHT.accent_soft)), "{x:?}");
    }
    v.set("theme.light.accent", "#336699");
    assert_eq!(Theme::build(&v).accent, h("#336699"), "a mode's own accent is a key too");
}

#[test]
fn the_derived_tones_match_the_designs_for_its_own_accents() {
    for (p, mode) in [(&DARK, "dark"), (&LIGHT, "light")] {
        let dark = mode == "dark";
        let ground = if dark { h(p.bg) } else { h(p.panel) };
        let s = soft(h(p.accent), ground, dark);
        assert!(near(s, h(p.accent_soft), 12.0 / 255.0), "{mode}: {s:?} near {}", p.accent_soft);
        assert_eq!(on(h(p.accent)), h(p.on_accent), "{mode}");
    }
}

#[test]
fn settings_change_mode_layout_scale_and_fonts() {
    let mut v = Values::default();
    v.set("theme.mode", "light");
    v.set("theme.layout.address_bar", "tile");
    v.set("theme.scale", "150");
    v.set("theme.font.ui", "Exo 2");
    v.set("theme.font.display", "figtree");
    v.set("theme.font.mono", "Noto Sans Mono");
    let t = Theme::build(&v);
    assert!(!t.dark);
    assert!((t.glow[3] - 0.14).abs() < 1e-6);
    assert_eq!(t.header_height, 72);
    assert_eq!(t.scale, 1.5);
    assert!((t.board - 1.6).abs() < 1e-9, "the board scale follows theme.scale");
    assert_eq!(t.fonts, Families { display: Typeface::Figtree, ui: Typeface::Exo2, mono: Typeface::JetBrainsMono });
    assert_eq!(t.font(Family::Ui, 13.0, Weight::Regular).typeface(), Typeface::Exo2, "Exo 2 stays selectable");
    let notes = t.font_notes();
    assert!(
        notes.len() == 1 && notes[0].contains("Noto Sans Mono") && notes[0].contains("JetBrains Mono"),
        "{notes:?}"
    );
}

#[test]
fn styles_are_board_sizes_at_the_output() {
    let t = Theme::default();
    let f = t.style(Style::Absicht);
    assert_eq!(f.typeface(), Typeface::Bricolage);
    let probe = Font::of(Typeface::Bricolage, 44.0, Weight::SemiBold, 4.0 / 3.0).spaced(-0.02);
    assert_eq!(f, probe, "a-absicht: 44 board px, 600, -0.02 em at 4/3");
    assert_eq!(t.style(Style::Label).typeface(), Typeface::JetBrainsMono);
    let l = spec(Style::Label);
    assert!(l.upper && l.family == Family::Mono && l.track > 0.0);
    assert_eq!(Theme::label(Style::Label, "Als Nächstes"), "ALS NÄCHSTES");
    assert_eq!(Theme::label(Style::Absicht, "Als Nächstes"), "Als Nächstes");
    let mut v = Values::default();
    v.set("theme.font.size", "30");
    assert!(Theme::build(&v).style(Style::Absicht).width("Bühne") > t.style(Style::Absicht).width("Bühne"));
}

#[test]
fn shadows_follow_the_mode_and_the_theme_compares_stably() {
    let d = Theme::default();
    assert_eq!(d.shadow_raised.0[0], ShadowLayer { dy: 0.0, blur: 0.0, spread: 1.0, color: d.line });
    assert_eq!((d.shadow_lifted.0[1].dy, d.shadow_lifted.0[1].blur, d.shadow_lifted.0[1].spread), (24.0, 48.0, -12.0));
    let l = Theme::build(&in_mode("light"));
    assert_eq!(l.shadow_raised.0[1].color, alpha(l.text, 0.18), "light shadows are ink, not black");
    // apply.rs drops every cached image when the theme differs: equal settings must give an equal theme (no NaN).
    assert_eq!(Theme::default(), Theme::default());
    assert_eq!(Theme::build(&in_mode("light")), l);
}

#[test]
fn garbage_values_take_the_default_or_are_clamped() {
    let d = Theme::default();
    let mut v = Values::default();
    for (k, x) in [
        ("theme.accent", "zzz"),
        ("theme.mode", "foo"),
        ("theme.scale", "0"),
        ("theme.font.size", "-5"),
        ("theme.font.ui", ""),
        ("theme.font.display", "\u{0}"),
        ("theme.spacing", "99999999999999999999"),
        ("theme.radius", "-1"),
        ("theme.glow", "1000"),
        ("theme.motion", "wild"),
        ("theme.dark.bg", "#12345"),
        ("theme.dark.text", "#ggg000"),
        ("theme.dark.border", "#ü1234"),
        ("theme.dark.kontext_2", "#ü12345"),
        ("theme.layout.address_bar", "sideways"),
        ("theme.layout.statusbar", "maybe"),
    ] {
        v.set(k, x);
    }
    let t = Theme::build(&v);
    assert_eq!(Theme::build(&v), t, "garbage gives a stable theme too (no NaN: apply.rs would redraw every message)");
    assert!(t.dark, "an unknown mode is dark (the default)");
    assert_eq!((t.accent, t.bg, t.text, t.line, t.kontext), (d.accent, d.bg, d.text, d.line, d.kontext));
    assert_eq!(t.fonts, d.fonts, "unknown font names keep the design's");
    assert_eq!(t.scale, 1.0, "scale 0 is clamped to 100 %");
    assert_eq!(t.text_k, 8.0 / 15.0, "font size -5 is clamped to 8");
    assert_eq!(t.gap, d.gap, "an unreadable number takes the default");
    assert_eq!((t.radius, t.radius_card), (0, 0), "radius -1 is clamped to 0");
    assert!(t.glow[3] <= 0.4 + 1e-6);
    assert_eq!(t.motion, Motion::Medium);
    assert!(t.layout.address_top && t.layout.statusbar);
    assert_eq!(t.cursor_size, 32, "WY-11: theme.cursor.size defaults to 32 logical px");
    v.set("theme.scale", "99999");
    v.set("theme.font.size", "99999");
    v.set("theme.cursor.size", "99999");
    let t = Theme::build(&v);
    assert_eq!((t.scale, t.text_k), (3.0, 48.0 / 15.0), "huge values are clamped");
    assert_eq!(t.cursor_size, 128);
}

fn palette_in_settingsd(src: &str, p: &Palette, mode: &str) {
    for (k, c) in p.keys() {
        assert!(src.contains(&format!("\"{k}\"")), "settingsd lacks theme.{mode}.{k}");
        assert!(src.contains(&format!("\"{c}\"")), "settingsd lacks {mode} {k} {c}");
    }
}

#[test]
fn settingsd_defaults_match_the_built_in_ones() {
    // Without settingsd (nested on CachyOS) and with it the screen must look the same: every palette key and
    // default in settingsd's schema matches bitscomp's.
    let src = include_str!("../../../settingsd/src/builtin.rs");
    palette_in_settingsd(src, &DARK, "dark");
    palette_in_settingsd(src, &LIGHT, "light");
    assert!(src.contains(&format!("text(\"{ACCENT}\")")), "theme.accent's default is the moss");
    for (k, d) in [
        ("theme.font.display", "Bricolage Grotesque"),
        ("theme.font.ui", "Figtree"),
        ("theme.font.mono", "JetBrains Mono"),
    ] {
        assert!(src.contains(&format!("\"{k}\"")) && src.contains(&format!("text(\"{d}\")")), "settingsd: {k} = {d}");
    }
    for k in ["theme.scanlines", "theme.layout.preset"] {
        assert!(src.contains(&format!("\"{k}\"")), "settingsd lacks {k}");
    }
    for k in [
        "theme.layout.address_bar",
        "theme.layout.favourites_bar",
        "theme.layout.workspaces_bar",
        "theme.layout.claude_sidebar",
        "theme.layout.statusbar",
        "desktop.favourites",
        "claude.name",
    ] {
        assert!(src.contains(k), "settingsd lacks {k}");
    }
}

#[test]
fn effects_gpu_is_accepted_and_drawn_like_cheap_and_scanlines_switch() {
    use crate::theme::Effects;
    let mut v = Values::default();
    let t = Theme::build(&v);
    assert_eq!((t.effects, t.effects_gpu, t.scanlines), (Effects::Cheap, false, true));
    v.set("theme.effects", "gpu");
    v.set("theme.scanlines", "false");
    let g = Theme::build(&v);
    assert_eq!((g.effects, g.effects.word()), (Effects::Gpu, "gpu"));
    assert!(!g.effects_gpu, "the GPU level is not drawn before a GPU renderer exists");
    assert!(!g.scanlines);
    // Everything drawn is the same: only the chosen level and the scanline switch differ.
    let same = Theme { effects: Effects::Cheap, scanlines: true, ..g };
    assert_eq!(same, t);
}
