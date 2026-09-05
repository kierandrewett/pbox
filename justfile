set shell := ["bash", "-uc"]

check:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

fmt:
    cargo fmt --all
