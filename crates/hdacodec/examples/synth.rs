//! Writes the synthetic fixture dumps: `cargo run --example synth --features synth -- [dir]`
//! (default dir `fixtures`). Once a real dump of a codec exists, it replaces the file and this
//! generator is no longer used for it.

fn main() -> std::io::Result<()> {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "fixtures".into());
    for name in ["alc269", "aa01"] {
        let dump = hdacodec::synth::fixture(name).expect("known fixture");
        let path = format!("{dir}/{name}-synthetic.txt");
        std::fs::write(&path, dump.to_text())?;
        println!("{path}: {} reads", dump.lines().len());
    }
    Ok(())
}
