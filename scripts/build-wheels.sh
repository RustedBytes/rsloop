#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_OUTPUT_DIR="dist/wheels"
DEFAULT_VERSIONS=(3.8 3.9 3.10 3.11 3.12 3.13 3.14 3.14t)

usage() {
  cat <<'EOF'
Build release wheels for rsloop with uv-managed Python interpreters.

Usage:
  scripts/build-wheels.sh [--out DIR] [--skip-python-install] [-- maturin args...]

Options:
  -o, --out DIR           Output directory for built wheels (default: dist/wheels)
      --skip-python-install
                          Reuse installed interpreters instead of running `uv python install`
  -h, --help              Show this help

Environment:
  RSLOOP_PYTHON_VERSIONS  Space-separated version list to override the defaults
                          (default: 3.8 3.9 3.10 3.11 3.12 3.13 3.14 3.14t)

Examples:
  scripts/build-wheels.sh
  scripts/build-wheels.sh --out wheelhouse
  RSLOOP_PYTHON_VERSIONS="3.12 3.13 3.14t" scripts/build-wheels.sh -- --features profiler
EOF
}

resolve_path() {
  local path="$1"
  if [[ "$path" = /* ]]; then
    printf '%s\n' "$path"
  else
    printf '%s/%s\n' "$ROOT_DIR" "$path"
  fi
}

OUTPUT_DIR="$DEFAULT_OUTPUT_DIR"
INSTALL_PYTHONS=1
MATURIN_ARGS=()

while (($#)); do
  case "$1" in
    -o|--out)
      if (($# < 2)); then
        echo "missing value for $1" >&2
        exit 1
      fi
      OUTPUT_DIR="$2"
      shift 2
      ;;
    --skip-python-install)
      INSTALL_PYTHONS=0
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    --)
      shift
      MATURIN_ARGS=("$@")
      break
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

if [[ -n "${RSLOOP_PYTHON_VERSIONS:-}" ]]; then
  read -r -a PYTHON_VERSIONS <<< "${RSLOOP_PYTHON_VERSIONS}"
else
  PYTHON_VERSIONS=("${DEFAULT_VERSIONS[@]}")
fi

OUTPUT_DIR="$(resolve_path "$OUTPUT_DIR")"

cd "$ROOT_DIR"
mkdir -p "$OUTPUT_DIR"

if (( INSTALL_PYTHONS )); then
  echo "Installing Python interpreters with uv: ${PYTHON_VERSIONS[*]}"
  uv python install "${PYTHON_VERSIONS[@]}"
fi

for version in "${PYTHON_VERSIONS[@]}"; do
  interpreter="$(uv python find "$version")"
  echo "Building release wheel for Python ${version} (${interpreter})"
  uv run \
    --no-project \
    --python "$interpreter" \
    --with maturin \
    maturin build \
    --release \
    --interpreter "$interpreter" \
    --out "$OUTPUT_DIR" \
    "${MATURIN_ARGS[@]}"
done

echo "Wheels written to $OUTPUT_DIR"
