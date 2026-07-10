test:
    cargo test --workspace

# Prove every crate stands alone (Law 2)
standalone:
    for c in reeve-server reeve-agent reeve-types margo-package desired-state revision-store device-api; do \
        cargo build -p $c || exit 1; \
    done

ui-dev:
    cd ui && npm run dev

# utoipa openapi.json -> orval-generated TS client + React Query hooks (D1)
gen-api:
    cargo run -p reeve-server -- openapi > ui/openapi.json
    cd ui && npm run gen-api

# Conformance: core loop with all extensions compiled out (E2)
conformance:
    cargo build -p reeve-agent --no-default-features
    cargo build -p reeve-server --no-default-features
    cargo test -p reeve-server --no-default-features

# vite build before cargo so build.rs embeds a fresh ui/dist
build:
    cd ui && npm run build
    cargo build --release -p reeve-server
