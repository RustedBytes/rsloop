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

test-frameworks: test-databases
    uv run --with uvicorn python tests/packages/uvicorn_test.py
    uv run --with daphne python tests/packages/daphne_test.py
    uv run --with hypercorn python tests/packages/hypercorn_test.py
    uv run --with mangum python tests/packages/mangum_test.py
    uv run --with granian --with litestar python tests/packages/litestar_granian_test.py
    uv run --with fastapi --with uvicorn python tests/packages/fastapi_test.py
    uv run --with starlette --with uvicorn python tests/packages/starlette_test.py
    uv run --with aiohttp python tests/packages/aiohttp_test.py
    uv run --with sanic python tests/packages/sanic_test.py
    uv run --with litestar --with uvicorn python tests/packages/litestar_test.py
    uv run --with django --with uvicorn python tests/packages/django_asgi_test.py
    uv run --with falcon --with uvicorn python tests/packages/falcon_test.py
    uv run --with quart --with hypercorn python tests/packages/quart_test.py
    uv run --with 'faststream[nats]' python tests/packages/faststream_test.py
    uv run --with anyio python tests/packages/anyio_test.py

test-databases:
    uv run --with django python tests/packages/django_orm_test.py
    uv run --with edgy python tests/packages/edgy_test.py
    uv run --with ormar python tests/packages/ormar_test.py
    uv run --with piccolo python tests/packages/piccolo_test.py
    uv run --with 'sqlalchemy[asyncio]' --with aiosqlite python tests/packages/sqlalchemy_test.py
    uv run --with sqlmodel --with aiosqlite python tests/packages/sqlmodel_test.py
    uv run --with tortoise-orm python tests/packages/tortoise_orm_test.py

# Beanie needs a MongoDB server. Override RSLOOP_MONGODB_URL when it isn't local.
test-beanie:
    uv run --with beanie python tests/packages/beanie_test.py

# Just the AnyIO checks, which is what CI runs as its own job.
test-anyio:
    uv run --with anyio python tests/packages/anyio_test.py

bench-real-world:
    uv run --with {{benchmark-backend}} python benches/workload_matrix.py
