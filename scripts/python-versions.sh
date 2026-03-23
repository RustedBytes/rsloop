#!/usr/bin/env bash

RSLOOP_DEFAULT_PYTHON_VERSIONS=(3.8 3.9 3.10 3.11 3.12 3.13 3.14 3.14t)

rsloop_target_python_request() {
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

rsloop_default_versions_for_target() {
  local target="$1"

  case "$target" in
    aarch64-pc-windows-msvc)
      # Windows ARM64 Python distributions are only available for newer CPython releases.
      printf '%s\n' 3.11 3.12 3.13 3.14 3.14t
      ;;
    *)
      printf '%s\n' "${RSLOOP_DEFAULT_PYTHON_VERSIONS[@]}"
      ;;
  esac
}

rsloop_load_python_versions() {
  local target="${1:-}"
  local version=""

  RSLOOP_PYTHON_VERSIONS=()
  if [[ -n "${RSLOOP_PYTHON_VERSIONS_OVERRIDE:-}" ]]; then
    read -r -a RSLOOP_PYTHON_VERSIONS <<< "${RSLOOP_PYTHON_VERSIONS_OVERRIDE}"
    return 0
  fi

  while IFS= read -r version; do
    RSLOOP_PYTHON_VERSIONS+=("$version")
  done < <(rsloop_default_versions_for_target "$target")
}
