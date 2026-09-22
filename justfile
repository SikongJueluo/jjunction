default:
    @just --list

# Build the project
build:
    cargo build

# Type-check without building artifacts
check:
    cargo check

# Run tests
test:
    cargo test

# Format code
fmt:
    cargo fmt

# Lint with clippy
lint:
    cargo clippy --all-targets -- -D warnings
