test:
    cargo test --workspace

# Sean's trick: prove every crate stands alone
standalone:
    for c in reeve-server reeve-agent reeve-types margo-package desired-state repo-store device-api; do \
        cargo build -p $c || exit 1; \
    done

ui-dev:
    cd ui && npm run dev

# NOT WIRED YET: utoipa isn't a dependency on device-api/reeve-server,
# and no openapi->TS codegen tool is chosen. Fill in once decided.
api-types:
    @echo "api-types: not wired yet (needs utoipa + a TS codegen tool)" && exit 1

# vite build before cargo so build.rs embeds a fresh ui/dist
build:
    cd ui && npm run build
    cargo build --release -p reeve-server
