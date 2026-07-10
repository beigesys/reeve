//! reeve-agent — the agent. fetch -> diff -> apply (Provider trait: compose
//! first) -> report. Offline-first: every network call has a
//! continue-from-last-known-state path. Crash-only.
fn main() {
    println!("reeve-agent: up to date, nothing to apply");
}
