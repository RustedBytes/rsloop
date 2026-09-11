set shell := ["bash", "-euo", "pipefail", "-c"]
set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

benchmark-backend := if os() == "windows" { "winloop" } else { "uvloop" }
python := if os() == "windows" { "python" } else { "python3" }

tls-test-certs outdir="tests/fixtures/tls":
    uv run --no-project python scripts/generate_test_tls_certs.py {{outdir}}

fmt:
    uv run ruff format .
    cargo fmt --all

clippy:
    uv run cargo clippy --all-targets --all-features -- -D warnings

test-rust:
    uv run python scripts/run_rust_tests.py

# Fast merge-gating proofs; `merge_` harnesses must fit the PR runtime budget.
kani-core:
    cargo kani --harness merge_ -j 2 --output-format terse

# Every proof harness, including longer bounded state sequences.
kani:
    cargo kani -j 2 --output-format terse

kani-list:
    cargo kani list

# Manual audit for over-constrained proof harnesses across the complete suite.
kani-coverage:
    cargo kani --coverage -Z source-coverage --output-format terse

# Faster source-coverage audit for merge-gating proofs.
kani-coverage-core:
    cargo kani --harness merge_ --coverage -Z source-coverage --output-format terse

test: tls-test-certs test-rust
    uv run python -u scripts/run_python_tests.py

test-tooling:
    uv run python -m pytest -m tooling tests/tooling

test-frameworks: test-databases
    uv run --with uvicorn python tests/integration/packages/uvicorn_test.py
    uv run --with daphne python tests/integration/packages/daphne_test.py
    uv run --with hypercorn python tests/integration/packages/hypercorn_test.py
    uv run --with mangum python tests/integration/packages/mangum_test.py
    uv run --with granian --with litestar python tests/integration/packages/litestar_granian_test.py
    uv run --with fastapi --with uvicorn python tests/integration/packages/fastapi_test.py
    uv run --with starlette --with uvicorn python tests/integration/packages/starlette_test.py
    uv run --with aiohttp python tests/integration/packages/aiohttp_test.py
    uv run --with sanic python tests/integration/packages/sanic_test.py
    uv run --with litestar --with uvicorn python tests/integration/packages/litestar_test.py
    uv run --with django --with uvicorn python tests/integration/packages/django_asgi_test.py
    uv run --with falcon --with uvicorn python tests/integration/packages/falcon_test.py
    uv run --with quart --with hypercorn python tests/integration/packages/quart_test.py
    uv run --with 'faststream[nats]' python tests/integration/packages/faststream_test.py
    uv run --with anyio python tests/integration/packages/anyio_test.py

test-databases:
    uv run --with django python tests/integration/packages/django_orm_test.py
    uv run --with edgy python tests/integration/packages/edgy_test.py
    uv run --with ormar python tests/integration/packages/ormar_test.py
    uv run --with piccolo python tests/integration/packages/piccolo_test.py
    uv run --with 'sqlalchemy[asyncio]' --with aiosqlite python tests/integration/packages/sqlalchemy_test.py
    uv run --with sqlmodel --with aiosqlite python tests/integration/packages/sqlmodel_test.py
    uv run --with tortoise-orm python tests/integration/packages/tortoise_orm_test.py

# Beanie needs a MongoDB server. Override RSLOOP_MONGODB_URL when it isn't local.
test-beanie:
    uv run --with beanie python tests/integration/packages/beanie_test.py

# Just the AnyIO checks, which is what CI runs as its own job.
test-anyio:
    uv run --with anyio python tests/integration/packages/anyio_test.py

bench-real-world:
    uv run --with {{benchmark-backend}} --with 'zuvloop; python_version >= "3.14"' python benches/workload_matrix.py

bench-granian:
    uv run --with granian --with {{benchmark-backend}} python benches/compare_granian.py
