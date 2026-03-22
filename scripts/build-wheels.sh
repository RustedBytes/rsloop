#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEFAULT_OUTPUT_DIR="dist/wheels"
DEFAULT_VERSIONS=(3.8 3.9 3.10 3.11 3.12 3.13 3.14 3.14t)

usage() {
  cat <<'EOF'
Build release wheels for rsloop with uv-managed Python interpreters.

Usage:
  scripts/build-wheels.sh [--out DIR] [--target TRIPLE] [--skip-python-install] [-- maturin args...]

Options:
  -o, --out DIR           Output directory for built wheels (default: dist/wheels)
  -t, --target TRIPLE     Rust compilation target to pass to maturin
      --skip-python-install
                          Reuse installed interpreters instead of running `uv python install`
  -h, --help              Show this help

Environment:
  RSLOOP_PYTHON_VERSIONS  Space-separated version list to override the defaults
                          (default: 3.8 3.9 3.10 3.11 3.12 3.13 3.14 3.14t)
  RSLOOP_RUST_TARGET      Rust compilation target to pass to maturin

Examples:
  scripts/build-wheels.sh
  scripts/build-wheels.sh --out wheelhouse
  scripts/build-wheels.sh --target aarch64-apple-darwin
  scripts/build-wheels.sh --target x86_64-pc-windows-msvc
  scripts/build-wheels.sh --target aarch64-pc-windows-msvc
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

target_python_request() {
  local version="$1"
  local target="$2"
  local arch=""
  local os=""
  local libc=""

  case "$target" in
    x86_64-unknown-linux-gnu)
      arch="x86_64"
      os="linux"
      libc="gnu"
      ;;
    aarch64-apple-darwin)
      arch="aarch64"
      os="macos"
      libc="none"
      ;;
    x86_64-apple-darwin)
      arch="x86_64"
      os="macos"
      libc="none"
      ;;
    x86_64-pc-windows-msvc)
      arch="x86_64"
      os="windows"
      libc="none"
      ;;
    aarch64-pc-windows-msvc)
      arch="aarch64"
      os="windows"
      libc="none"
      ;;
    *)
      printf '%s\n' "$version"
      return 0
      ;;
  esac

  printf 'cpython-%s-%s-%s-%s\n' "$version" "$os" "$arch" "$libc"
}

default_versions_for_target() {
  local target="$1"

  case "$target" in
    aarch64-pc-windows-msvc)
      # Windows ARM64 Python distributions are only available for newer CPython releases.
      printf '%s\n' 3.11 3.12 3.13 3.14 3.14t
      ;;
    *)
      printf '%s\n' "${DEFAULT_VERSIONS[@]}"
      ;;
  esac
}

host_rust_target() {
  rustc -vV | sed -n 's/^host: //p'
}

OUTPUT_DIR="$DEFAULT_OUTPUT_DIR"
INSTALL_PYTHONS=1
RUST_TARGET="${RSLOOP_RUST_TARGET:-}"
HOST_RUST_TARGET=""
IS_CROSS_COMPILE=0
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
    -t|--target)
      if (($# < 2)); then
        echo "missing value for $1" >&2
        exit 1
      fi
      RUST_TARGET="$2"
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
  readarray -t PYTHON_VERSIONS < <(default_versions_for_target "$RUST_TARGET")
fi

OUTPUT_DIR="$(resolve_path "$OUTPUT_DIR")"

cd "$ROOT_DIR"
mkdir -p "$OUTPUT_DIR"

if [[ -n "$RUST_TARGET" ]]; then
  HOST_RUST_TARGET="$(host_rust_target)"
  if [[ "$RUST_TARGET" != "$HOST_RUST_TARGET" ]]; then
    IS_CROSS_COMPILE=1
  fi
fi

if (( INSTALL_PYTHONS )); then
  if [[ -n "$RUST_TARGET" ]]; then
    python_requests=()
    for version in "${PYTHON_VERSIONS[@]}"; do
      python_requests+=("$(target_python_request "$version" "$RUST_TARGET")")
    done
    echo "Installing Python interpreters with uv for ${RUST_TARGET}: ${python_requests[*]}"
    uv python install "${python_requests[@]}"
  else
    echo "Installing Python interpreters with uv: ${PYTHON_VERSIONS[*]}"
    uv python install "${PYTHON_VERSIONS[@]}"
  fi
fi

for version in "${PYTHON_VERSIONS[@]}"; do
  python_request="$version"
  interpreter_selector=""

  if [[ -n "$RUST_TARGET" ]]; then
    python_request="$(target_python_request "$version" "$RUST_TARGET")"
    if (( IS_CROSS_COMPILE )); then
      interpreter_selector="python${version}"
    else
      interpreter_selector="$(uv python find "$python_request")"
    fi
    echo "Building release wheel for Python ${version} targeting ${RUST_TARGET} (${interpreter_selector})"
  else
    interpreter_selector="$(uv python find "$version")"
    echo "Building release wheel for Python ${version} (${interpreter_selector})"
  fi

  maturin_cmd=(
    uv run
    --no-project
    --python "$python_request"
    --with maturin
    maturin build
    --release
    --interpreter "$interpreter_selector"
    --out "$OUTPUT_DIR"
  )
  if [[ -n "$RUST_TARGET" ]]; then
    maturin_cmd+=(
      --target "$RUST_TARGET"
    )
  fi
  if ((${#MATURIN_ARGS[@]})); then
    maturin_cmd+=("${MATURIN_ARGS[@]}")
  fi
  "${maturin_cmd[@]}"
done

echo "Wheels written to $OUTPUT_DIR"
