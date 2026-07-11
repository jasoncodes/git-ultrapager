use std::env;
use std::path::PathBuf;

#[path = "build/syntax_candidates.rs"]
mod syntax_candidates;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build/syntax_candidates.rs");

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"))
        .join("syntax-with-continuations.packdump");
    let asset = syntax_candidates::build();
    syntect::dumps::dump_to_uncompressed_file(&asset, output)
        .expect("generated syntax asset can be written to OUT_DIR");
}
