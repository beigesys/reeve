# DECISIONS-MADE — judgment calls made during the autonomous build

One line each: what, where, which doc informed it.

- rusqlite gains the `session` feature at workspace level (Cargo.toml) — required by D16 changeset tier (docs/decisions/storage.md).
- crates/repo-store renamed to crates/revision-store, gix removed from workspace — direct execution of D13 (docs/decisions/delivery.md).
