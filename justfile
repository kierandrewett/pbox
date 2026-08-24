set shell := ["bash", "-uc"]

check:
    cargo check --workspace

test:
    cargo test -p pbox-core

fmt:
    cargo fmt --all
