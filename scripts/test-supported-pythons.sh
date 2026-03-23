#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "${ROOT_DIR}/scripts/python-versions.sh"

BUILD_MODE="release"
INSTALL_PYTHONS=1
GENERATE_TLS_CERTS=1

usage() {
  cat <<'EOF'
Run the rsloop test suite against all supported uv-managed Python interpreters.

Usage:
  scripts/test-supported-pythons.sh [--debug] [--skip-python-install] [--skip-tls-certs]

Options:
      --debug               Build with `maturin develop` instead of `maturin develop --release`
      --skip-python-install Reuse installed interpreters instead of running `uv python install`
      --skip-tls-certs      Assume `tests/fixtures/tls` already exists
  -h, --help                Show this help

Environment:
  RSLOOP_PYTHON_VERSIONS    Space-separated version list to override the defaults
                            (default: 3.8 3.9 3.10 3.11 3.12 3.13 3.14 3.14t)

Examples:
  scripts/test-supported-pythons.sh
  RSLOOP_PYTHON_VERSIONS="3.12 3.13 3.14t" scripts/test-supported-pythons.sh
  scripts/test-supported-pythons.sh --debug
EOF
}

while (($#)); do
  case "$1" in
    --debug)
      BUILD_MODE="debug"
      shift
      ;;
    --skip-python-install)
      INSTALL_PYTHONS=0
      shift
      ;;
    --skip-tls-certs)
      GENERATE_TLS_CERTS=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

RSLOOP_PYTHON_VERSIONS_OVERRIDE="${RSLOOP_PYTHON_VERSIONS:-}"
rsloop_load_python_versions
PYTHON_VERSIONS=("${RSLOOP_PYTHON_VERSIONS[@]}")

cd "$ROOT_DIR"

if (( GENERATE_TLS_CERTS )); then
  echo "Generating TLS test certificates"
  "${ROOT_DIR}/scripts/generate-test-tls-certs.sh"
fi

if (( INSTALL_PYTHONS )); then
  echo "Installing Python interpreters with uv: ${PYTHON_VERSIONS[*]}"
  uv python install "${PYTHON_VERSIONS[@]}"
fi

for version in "${PYTHON_VERSIONS[@]}"; do
  echo "Running tests for Python ${version}"
  uv run \
    --no-project \
    --python "$version" \
    --with maturin \
    python - "$ROOT_DIR" "$BUILD_MODE" <<'PY'
import subprocess
import sys

root_dir = sys.argv[1]
build_mode = sys.argv[2]

maturin_cmd = ["maturin", "develop"]
if build_mode == "release":
    maturin_cmd.append("--release")

subprocess.run(maturin_cmd, cwd=root_dir, check=True)
subprocess.run(
    [sys.executable, "-m", "unittest", "discover", "-s", "tests"],
    cwd=root_dir,
    check=True,
)
PY
done

echo "Tests passed for: ${PYTHON_VERSIONS[*]}"
