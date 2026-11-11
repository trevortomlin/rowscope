//! Embeds every SVG in assets/icons into the binary.

use std::fmt::Write;

fn main() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icons");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "svg"))
        .collect();
    names.sort();
    let mut out = String::from("&[\n");
    for p in names {
        let stem = p.file_stem().unwrap().to_string_lossy();
        writeln!(
            out,
            "    ({stem:?}, \"icons/{stem}.svg\", include_bytes!({:?}) as &[u8]),",
            p.display()
        )
        .unwrap();
    }
    out.push(']');
    let dest = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("icons.rs");
    std::fs::write(dest, out).unwrap();
}
