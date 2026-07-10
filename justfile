test:
    cargo test --workspace

# Sean's trick: prove every crate stands alone
standalone:
    for c in reeve reeve-agent reeve-types margo-package desired-state repo-store device-api; do \
        cargo build -p $c || exit 1; \
    done
