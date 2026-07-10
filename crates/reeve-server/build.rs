//! Placeholder: once ui/ is embedded via rust-embed, this should hard-fail
//! when ui/dist is missing/stale rather than silently packing an empty
//! embed. Softened to a warning for now since ui/dist isn't produced by
//! anything yet — tighten to `panic!` once `just build` exists and runs
//! in CI.
use std::path::Path;

fn main() {
    let ui_dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist");
    println!("cargo:rerun-if-changed={}", ui_dist.display());

    if !ui_dist.exists() {
        println!(
            "cargo:warning=ui/dist not found at {} — run `just build` (or `cd ui && npm run build`) before shipping",
            ui_dist.display()
        );
    }
}
