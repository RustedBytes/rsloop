#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "${ROOT_DIR}/scripts/python-versions.sh"

DEFAULT_PGO_SCENARIOS="http_keepalive,tls_http,websocket_messages,websocket_tls,mixed_streams,bulk_transfer,idle_connections"
BUILD_WHEEL_ARGS=("$@")
RUST_TARGET="${RSLOOP_RUST_TARGET:-}"
INSTALL_PYTHONS=1
MATURIN_ARGS=()

usage() {
  cat <<'EOF'
Build PGO-optimized release wheels for rsloop.

Usage:
  scripts/build-pgo-wheels.sh [build-wheels options] [-- maturin args...]

This wrapper accepts the same options as scripts/build-wheels.sh. For each
requested Python ABI, it builds an instrumented native wheel, trains it with
representative traffic, merges the LLVM profiles, and builds that ABI's final
wheel with its matching profile.

Environment:
  RSLOOP_PGO_SCENARIOS    Comma-separated workload-matrix training scenarios

PGO requires a native target and the Rust llvm-tools-preview component.
EOF
}

host_rust_target() {
  rustc -vV | sed -n 's/^host: //p'
}

native_path() {
  case "${OSTYPE:-}" in
    msys*|cygwin*) cygpath -m "$1" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

shell_path() {
  case "${OSTYPE:-}" in
    msys*|cygwin*) cygpath -u "$1" ;;
    *) printf '%s\n' "$1" ;;
  esac
}

while (($#)); do
  case "$1" in
    -o|--out)
      (($# >= 2)) || { echo "missing value for $1" >&2; exit 1; }
      shift 2
      ;;
    -t|--target)
      (($# >= 2)) || { echo "missing value for $1" >&2; exit 1; }
      RUST_TARGET="$2"
      shift 2
      ;;
    --skip-python-install)
      INSTALL_PYTHONS=0
      shift
      ;;
    -h|--help)
      usage
      printf '\nUnderlying wheel-builder options:\n\n'
      "${ROOT_DIR}/scripts/build-wheels.sh" --help
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

cd "$ROOT_DIR"
HOST_RUST_TARGET="$(host_rust_target)"
if [[ -n "$RUST_TARGET" && "$RUST_TARGET" != "$HOST_RUST_TARGET" ]]; then
  echo "PGO requires a native target because the instrumented wheel must run during training" >&2
  echo "host target: ${HOST_RUST_TARGET}; requested target: ${RUST_TARGET}" >&2
  exit 1
fi
# An explicit --target prevents RUSTFLAGS instrumentation from reaching host
# build scripts and proc macros, even when building for the native platform.
RUST_TARGET="${RUST_TARGET:-$HOST_RUST_TARGET}"
if [[ "$RUST_TARGET" == *-apple-darwin ]]; then
  export MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-13.0}"
fi
if [[ "$RUST_TARGET" == "aarch64-pc-windows-msvc" ]]; then
  # rustc profile-generate binaries currently crash on native Windows ARM64:
  # https://github.com/rust-lang/rust/issues/156675
  echo "PGO is unavailable on Windows ARM64; building a normal fat-LTO release wheel"
  export RUSTFLAGS="${RUSTFLAGS:-} -Clink-arg=/arm64hazardfree"
  exec "${ROOT_DIR}/scripts/build-wheels.sh" "${BUILD_WHEEL_ARGS[@]}"
fi

RSLOOP_PYTHON_VERSIONS_OVERRIDE="${RSLOOP_PYTHON_VERSIONS:-}"
rsloop_load_python_versions "$RUST_TARGET"
PYTHON_VERSIONS=("${RSLOOP_PYTHON_VERSIONS[@]}")
PGO_SCENARIOS="${RSLOOP_PGO_SCENARIOS:-$DEFAULT_PGO_SCENARIOS}"

rust_sysroot="$(shell_path "$(rustc --print sysroot)")"
llvm_profdata="${rust_sysroot}/lib/rustlib/${HOST_RUST_TARGET}/bin/llvm-profdata"
if [[ "${OSTYPE:-}" == msys* || "${OSTYPE:-}" == cygwin* ]]; then
  llvm_profdata+=".exe"
fi
if [[ ! -x "$llvm_profdata" ]]; then
  echo "llvm-profdata was not found at ${llvm_profdata}" >&2
  echo "install it with: rustup component add llvm-tools-preview" >&2
  exit 1
fi

PGO_WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/rsloop-wheel-pgo.XXXXXX")"
cleanup() {
  if [[ -n "${PGO_WORK_DIR:-}" && -d "$PGO_WORK_DIR" ]]; then
    rm -rf -- "$PGO_WORK_DIR"
  fi
}
trap cleanup EXIT

PROFILE_DIR="${PGO_WORK_DIR}/profiles"
TARGET_GENERATE="$(native_path "${PGO_WORK_DIR}/target-generate")"
TARGET_USE="$(native_path "${PGO_WORK_DIR}/target-use")"
MERGED_PROFILE="${PGO_WORK_DIR}/merged.profdata"
mkdir -p "$PROFILE_DIR"
BASE_RUSTFLAGS="${RUSTFLAGS:-}"
GENERATE_RUSTFLAGS="${BASE_RUSTFLAGS} -Cprofile-generate=$(native_path "$PROFILE_DIR")"
USE_RUSTFLAGS="${BASE_RUSTFLAGS} -Cprofile-use=$(native_path "$MERGED_PROFILE") -Cllvm-args=-pgo-warn-missing-function"

for version in "${PYTHON_VERSIONS[@]}"; do
  python_request="$version"
  if [[ -n "$RUST_TARGET" ]]; then
    python_request="$(rsloop_target_python_request "$version" "$RUST_TARGET")"
  fi
  if (( INSTALL_PYTHONS )); then
    uv python install "$python_request"
  fi

  version_key="${version//[^a-zA-Z0-9]/_}"
  version_dir="${PGO_WORK_DIR}/${version_key}"
  wheel_dir="${version_dir}/instrumented-wheel"
  tls_dir="${version_dir}/tls"
  training_venv="${version_dir}/venv"
  mkdir -p "$wheel_dir"
  find "$PROFILE_DIR" -maxdepth 1 -type f -name '*.profraw' -delete

  instrumented_build=(uv run --no-project --python "$python_request" --with maturin maturin build --release --interpreter "$(uv python find "$python_request")" --out "$wheel_dir")
  if [[ -n "$RUST_TARGET" ]]; then
    instrumented_build+=(--target "$RUST_TARGET")
  fi
  if ((${#MATURIN_ARGS[@]})); then
    instrumented_build+=("${MATURIN_ARGS[@]}")
  fi
  echo "Building instrumented PGO training wheel for Python ${version}"
  CARGO_TARGET_DIR="$TARGET_GENERATE" RUSTFLAGS="$GENERATE_RUSTFLAGS" "${instrumented_build[@]}"

  shopt -s nullglob
  instrumented_wheels=("$wheel_dir"/*.whl)
  shopt -u nullglob
  if ((${#instrumented_wheels[@]} != 1)); then
    echo "expected one instrumented training wheel, found ${#instrumented_wheels[@]}" >&2
    exit 1
  fi

  uv venv --no-project --python "$python_request" "$training_venv"
  if [[ -x "$training_venv/Scripts/python.exe" ]]; then
    training_python="$training_venv/Scripts/python.exe"
  else
    training_python="$training_venv/bin/python"
  fi
  uv pip install --python "$training_python" "${instrumented_wheels[0]}"
  "$training_python" scripts/generate_test_tls_certs.py "$tls_dir"

  echo "Training Python ${version} PGO with sustained representative network traffic"
  LLVM_PROFILE_FILE="$(native_path "$PROFILE_DIR")/runtime-%p-%m.profraw" \
    "$training_python" benches/workload_matrix.py --loops rsloop --scenarios "$PGO_SCENARIOS" --sustained --tls-dir "$tls_dir"
  echo "Training Python ${version} PGO with callbacks, tasks, and TCP streams"
  LLVM_PROFILE_FILE="$(native_path "$PROFILE_DIR")/runtime-%p-%m.profraw" \
    "$training_python" benches/compare_event_loops.py --loops rsloop

  shopt -s nullglob
  raw_profiles=("$PROFILE_DIR"/*.profraw)
  shopt -u nullglob
  if ((${#raw_profiles[@]} == 0)); then
    echo "PGO training produced no .profraw files for Python ${version}" >&2
    exit 1
  fi

  echo "Merging ${#raw_profiles[@]} LLVM profile shards for Python ${version}"
  "$llvm_profdata" merge -o "$MERGED_PROFILE" "${raw_profiles[@]}"
  echo "Building final Python ${version} wheel with its matching PGO profile"
  RSLOOP_PYTHON_VERSIONS="$version" \
    CARGO_TARGET_DIR="$TARGET_USE" \
    RUSTFLAGS="$USE_RUSTFLAGS" \
    "${ROOT_DIR}/scripts/build-wheels.sh" --target "$HOST_RUST_TARGET" "${BUILD_WHEEL_ARGS[@]}"
  rm -rf -- "$version_dir"
done
