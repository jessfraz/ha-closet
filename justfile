set shell := ["bash", "-euo", "pipefail", "-c"]

default: check

rust:
    cargo fmt --check
    cargo clippy --locked --all --benches --tests --examples --all-features -- -D warnings
    cargo test --locked --all-features

ha:
    uv run --frozen python scripts/build-integration
    cargo build --locked --bins
    uv sync --frozen
    uv run --frozen ruff check custom_components tests_ha tests_web scripts
    uv run --frozen ruff format --check custom_components tests_ha tests_web scripts
    uv run --frozen ty check --error-on-warning custom_components tests_ha tests_web
    uv run --frozen playwright install chromium
    CLOSET_TEST_BINARY="{{justfile_directory()}}/target/debug/closet" uv run --frozen pytest tests_ha tests_web

repository:
    scripts/check-public-tree
    scripts/check-release-version
    actionlint
    sh -n docker-entrypoint.sh
    bash -n scripts/container-smoke.sh scripts/private-data-smoke.sh
    shellcheck docker-entrypoint.sh scripts/container-smoke.sh scripts/private-data-smoke.sh

openapi:
    cargo run --locked --bin closet -- --openapi openapi/api.json

check: rust ha repository

security:
    cargo deny check advisories licenses sources

container:
    docker build --tag ha-closet:smoke .
    scripts/container-smoke.sh ha-closet:smoke

integration-zip:
    uv run --frozen python scripts/build-integration --check-brand closet --output work
