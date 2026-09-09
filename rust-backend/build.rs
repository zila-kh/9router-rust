use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/provider-catalog.json");
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let path = manifest.join("assets/provider-catalog.json");
    let raw = fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let value: serde_json::Value = serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("invalid provider catalog {}: {e}", path.display()));
    let count = value
        .get("registry")
        .and_then(serde_json::Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if count == 0 {
        panic!(
            "provider catalog is empty; install this overlay into the pinned 9Router checkout or run `node scripts/export-rust-catalog.mjs` from that checkout before building"
        );
    }
}
