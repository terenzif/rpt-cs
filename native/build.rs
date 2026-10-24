use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let manifest = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("Cargo.toml");
    let text = fs::read_to_string(manifest).unwrap();
    let rev = text
        .lines()
        .find_map(|line| {
            line.split("rev = \"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
        })
        .unwrap_or("unknown");
    let short: String = rev.chars().take(7).collect();
    println!("cargo:rustc-env=RPT_RS_REV={short}");
    println!("cargo:rerun-if-changed=Cargo.toml");
}