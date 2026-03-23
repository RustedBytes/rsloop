set shell := ["bash", "-euo", "pipefail", "-c"]

tls-test-certs outdir="tests/fixtures/tls":
    ./scripts/generate-test-tls-certs.sh {{outdir}}

fmt:
    uv run ruff format .
    cargo fmt --all

test:
    uv run python -m unittest discover -s tests
