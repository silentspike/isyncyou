# iSyncYou developer tasks. Run `just` for the list.
default:
    @just --list

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all --check

lint:
    cargo clippy --workspace --all-targets -- -D warnings

test:
    cargo test --workspace

deny:
    cargo deny check

# Full pre-push gate
check: fmt-check lint test
