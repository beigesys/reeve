//! reeve — the server. Renders the tree into per-device repos, serves
//! the device API and UI. SQLite (WAL) for runtime state; git repos are
//! the source of truth. Crash-only: kill -9 mid-render must be safe.
fn main() {
    println!("reeve: no devices enrolled");
}
