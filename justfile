set shell := ["bash", "-uc"]

check:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace
    python3 -m unittest discover -s scripts -p 'test_*.py'

fmt:
    cargo fmt --all

# Real preparation, offline systemd presets and authenticated RPC; cleans test-owned Docker resources.
test-images *args:
    python3 scripts/test-images.py {{args}}

# Requires the Rust musl target and a musl C compiler (for example musl-gcc).
build-agent target="x86_64-unknown-linux-musl":
    cargo build --locked --release --target {{target}} -p pbox-agent
    mkdir -p target/release
    install -m 755 target/{{target}}/release/pbox-agent target/release/pbox-agent

# The CLI is native; the guest agent is portable across glibc and musl images.
build:
    cargo build --locked --release -p pbox
    just build-agent
