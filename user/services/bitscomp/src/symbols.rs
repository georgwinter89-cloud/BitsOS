//! GPY (desk 15.11.): the symbol fonts V2 ships as files beside Noto Sans Mono (user/packaging/manifest.txt) for the
//! characters neither Noto Sans Mono nor bitscomp's built-in families have. Claude Code's interface showed empty
//! boxes in foot and in the tab titles for ✳ (title; its spinner ◐ ◑ lacks only in bitscomp's titles), ❯ (prompt),
//! ✻ ("Baked for"), ⏵ ("auto mode on"), ⚠ (the MCP notice) and ⎿ (tool results). foot takes them as fallback
//! fonts by family name (`terminal::with_symbols`), bitscomp's own text by file (`text`), in this order.

/// (fontconfig family, file on V2): the files are the host's (CachyOS noto-fonts, OFL-1.1; ttf-dejavu, Bitstream
/// Vera licence), copied unchanged by the package step.
pub const FONTS: [(&str, &str); 3] = [
    ("Noto Sans Symbols 2", "/usr/share/fonts/noto/NotoSansSymbols2-Regular.ttf"),
    ("Noto Sans Symbols", "/usr/share/fonts/noto/NotoSansSymbols-Regular.ttf"),
    ("DejaVu Sans Mono", "/usr/share/fonts/TTF/DejaVuSansMono.ttf"),
];

/// The characters of Claude Code 2.1.325's interface that Noto Sans Mono lacks (its figures table, spinner, title
/// and status icons on Linux, the ✓ ✗ of its status lines), all of which the files above cover. Missing still:
/// U+29C9 ⧉ (only in front of an IDE selection, which V2 has no IDE for).
#[cfg(test)]
pub const CLAUDE_CODE: &str = "✳❯✻⏵⚠⎿✢✽⏸⏺✔✘✓✗✕↯⑂⏎✦⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏⠿⌕↳↻★☐☒☰♦☺Ⓘⓞⓧ⬢";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_files_cover_claude_codes_signs_noto_sans_mono_lacks() {
        let faces: Vec<fontdue::Font> = FONTS
            .iter()
            .map(|(name, p)| {
                let b = std::fs::read(p).unwrap_or_else(|e| panic!("{name}: {p}: {e} (noto-fonts, ttf-dejavu)"));
                fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).expect("parses")
            })
            .collect();
        let mono = std::fs::read("/usr/share/fonts/noto/NotoSansMono-Regular.ttf").expect("Noto Sans Mono");
        let mono = fontdue::Font::from_bytes(mono, fontdue::FontSettings::default()).expect("parses");
        for c in CLAUDE_CODE.chars() {
            assert!(!mono.has_glyph(c), "{c} U+{:04X}: Noto Sans Mono has it, drop it from the list", c as u32);
            assert!(faces.iter().any(|f| f.has_glyph(c)), "{c} U+{:04X}: no symbol font has it", c as u32);
        }
    }
}
