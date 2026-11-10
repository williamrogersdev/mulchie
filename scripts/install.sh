#!/bin/sh
# Mulchie installer.
#
#   curl -fsSL https://mulchie.com/install.sh | sh
#
# Environment:
#   MULCHIE_VERSION      Version to install, such as 0.3.0 or v0.3.0. Default: latest.
#   MULCHIE_INSTALL_DIR  Where to put the binary. Default: $HOME/.local/bin.
#   MULCHIE_BASE_URL     Directory URL that holds mulchie-<target>.tar.gz and its .sha256.
#                        Default: the GitHub release download URL. Used by tests (file://).
#
# The binary is checked against the published SHA-256 before it is installed.
# Nothing here needs sudo.

set -eu

REPO="williamrogersdev/mulchie"

say() {
  printf '%s\n' "$*"
}

err() {
  printf 'mulchie install: %s\n' "$*" >&2
}

die() {
  err "$*"
  exit 1
}

have() {
  command -v "$1" >/dev/null 2>&1
}

# Print the Rust target triple for this machine, or fail.
detect_target() {
  os="$(uname -s)"
  arch="$(uname -m)"
  case "$os" in
    Darwin) os_part="apple-darwin" ;;
    Linux) os_part="unknown-linux-gnu" ;;
    *) die "unsupported operating system: $os. Mulchie supports macOS and Linux." ;;
  esac
  case "$arch" in
    arm64 | aarch64) arch_part="aarch64" ;;
    x86_64 | amd64) arch_part="x86_64" ;;
    *) die "unsupported architecture: $arch. Mulchie supports arm64 and x86_64." ;;
  esac
  printf '%s-%s\n' "$arch_part" "$os_part"
}

# fetch URL DEST
fetch() {
  if have curl; then
    curl --fail --silent --show-error --location --proto '=https,file' "$1" --output "$2" ||
      die "download failed: $1"
  elif have wget; then
    wget --quiet --output-document="$2" "$1" || die "download failed: $1"
  else
    die "need curl or wget to download files"
  fi
}

sha256_of() {
  if have shasum; then
    shasum -a 256 "$1" | awk '{print $1}'
  elif have sha256sum; then
    sha256sum "$1" | awk '{print $1}'
  else
    die "need shasum or sha256sum to verify the download"
  fi
}

main() {
  version="${MULCHIE_VERSION:-latest}"
  install_dir="${MULCHIE_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}"

  target="$(detect_target)"
  asset="mulchie-${target}.tar.gz"

  if [ -n "${MULCHIE_BASE_URL:-}" ]; then
    base="${MULCHIE_BASE_URL%/}"
  elif [ "$version" = "latest" ]; then
    base="https://github.com/${REPO}/releases/latest/download"
  else
    base="https://github.com/${REPO}/releases/download/v${version#v}"
  fi

  tmp="$(mktemp -d "${TMPDIR:-/tmp}/mulchie-install.XXXXXX")" || die "could not create a temporary directory"
  trap 'rm -rf "$tmp"' EXIT INT TERM HUP

  say "Downloading ${asset} (${version})"
  fetch "${base}/${asset}" "${tmp}/${asset}"
  fetch "${base}/${asset}.sha256" "${tmp}/${asset}.sha256"

  expected="$(awk 'NR == 1 {print $1}' "${tmp}/${asset}.sha256")"
  [ -n "$expected" ] || die "the checksum file is empty"
  actual="$(sha256_of "${tmp}/${asset}")"
  if [ "$expected" != "$actual" ]; then
    err "checksum mismatch for ${asset}"
    err "  expected: ${expected}"
    err "  actual:   ${actual}"
    die "nothing was installed"
  fi
  say "Checksum OK"

  mkdir "${tmp}/extract"
  tar -xzf "${tmp}/${asset}" -C "${tmp}/extract" || die "could not unpack ${asset}"
  bin="$(find "${tmp}/extract" -type f -name mulchie | head -n 1)"
  [ -n "$bin" ] || die "the archive does not contain a mulchie binary"

  mkdir -p "$install_dir" || die "could not create ${install_dir}"
  staged="${install_dir}/.mulchie.new.$$"
  cp "$bin" "$staged" || die "could not write to ${install_dir}"
  chmod 755 "$staged"
  mv -f "$staged" "${install_dir}/mulchie" || {
    rm -f "$staged"
    die "could not install to ${install_dir}/mulchie"
  }

  say "Installed mulchie to ${install_dir}/mulchie"

  case ":${PATH}:" in
    *":${install_dir}:"*) ;;
    *)
      say ""
      say "${install_dir} is not on your PATH. Add it with:"
      say "  export PATH=\"${install_dir}:\$PATH\""
      say "Put that line in your shell profile to keep it."
      ;;
  esac

  say ""
  say "Next, run: mulchie doctor"
}

main "$@"
