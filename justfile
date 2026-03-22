set shell := ["bash", "-euo", "pipefail", "-c"]

tls-test-certs outdir="tests/fixtures/tls":
    ./scripts/generate-test-tls-certs.sh {{outdir}}
