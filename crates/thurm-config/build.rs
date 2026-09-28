//! Embeds the theme collection in `themes/` (one file per theme, named after it) as a sorted
//! `(name, text)` table.

use std::fmt::Write;
use std::path::Path;

fn main() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("themes");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("themes/")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n != "LICENSE" && !n.starts_with('.'))
        .collect();
    names.sort_by_key(|n| n.to_lowercase());
    let mut out = String::from("pub static THEMES: &[(&str, &str)] = &[\n");
    for n in &names {
        let path = dir.join(n);
        writeln!(
            out,
            "    ({n:?}, include_str!({:?})),",
            path.display().to_string()
        )
        .unwrap();
    }
    out.push_str("];\n");
    let dest = Path::new(&std::env::var("OUT_DIR").unwrap()).join("themes.rs");
    std::fs::write(dest, out).unwrap();
}
