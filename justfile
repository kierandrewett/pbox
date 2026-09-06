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
