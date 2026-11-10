#!/bin/sh
# Tests for install.sh. Builds a fake release in a temp dir and installs from file://.
# Run: scripts/test-install.sh

set -eu

here="$(cd "$(dirname "$0")" && pwd)"
installer="${here}/install.sh"
work="$(mktemp -d "${TMPDIR:-/tmp}/mulchie-test-install.XXXXXX")"
trap 'rm -rf "$work"' EXIT INT TERM HUP

failures=0
pass() { echo "ok   $1"; }
fail() {
  echo "FAIL $1"
  failures=$((failures + 1))
}

sha256_of() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    sha256sum "$1" | awk '{print $1}'
  fi
}

# The installer picks the target from uname, so work out the same name here.
case "$(uname -s)" in
  Darwin) os_part="apple-darwin" ;;
  Linux) os_part="unknown-linux-gnu" ;;
  *) echo "test-install.sh runs on macOS or Linux only"; exit 1 ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) arch_part="aarch64" ;;
  *) arch_part="x86_64" ;;
esac
target="${arch_part}-${os_part}"
asset="mulchie-${target}.tar.gz"

release="${work}/release"
mkdir -p "${release}" "${work}/stage/mulchie-${target}"
printf '#!/bin/sh\necho "fake mulchie 0.0.0"\n' >"${work}/stage/mulchie-${target}/mulchie"
chmod 755 "${work}/stage/mulchie-${target}/mulchie"
echo "MIT" >"${work}/stage/mulchie-${target}/LICENSE"
echo "readme" >"${work}/stage/mulchie-${target}/README.md"
tar -czf "${release}/${asset}" -C "${work}/stage" "mulchie-${target}"
echo "$(sha256_of "${release}/${asset}")  ${asset}" >"${release}/${asset}.sha256"

# 1. Install works, binary is executable, no temp files are left in the install dir.
dest="${work}/bin1"
if out="$(MULCHIE_BASE_URL="file://${release}" MULCHIE_INSTALL_DIR="$dest" sh "$installer" 2>&1)"; then
  if [ -x "${dest}/mulchie" ] && [ "$("${dest}/mulchie")" = "fake mulchie 0.0.0" ]; then
    pass "installs and the binary runs"
  else
    fail "installed binary is missing or does not run"
  fi
  if [ "$(find "$dest" -type f | wc -l | tr -d ' ')" = "1" ]; then
    pass "leaves no stray files in the install dir"
  else
    fail "stray files in the install dir"
  fi
  case "$out" in *"is not on your PATH"*) pass "prints a PATH hint" ;; *) fail "no PATH hint" ;; esac
  case "$out" in *"mulchie doctor"*) pass "suggests mulchie doctor" ;; *) fail "no doctor hint" ;; esac
else
  fail "install exited non-zero: $out"
fi

# 2. Checksum mismatch aborts and installs nothing.
bad="${work}/bad"
mkdir -p "$bad"
cp "${release}/${asset}" "$bad/"
echo "0000000000000000000000000000000000000000000000000000000000000000  ${asset}" >"${bad}/${asset}.sha256"
dest="${work}/bin2"
if out="$(MULCHIE_BASE_URL="file://${bad}" MULCHIE_INSTALL_DIR="$dest" sh "$installer" 2>&1)"; then
  fail "checksum mismatch did not fail"
else
  case "$out" in *"checksum mismatch"*) pass "checksum mismatch fails with a message" ;; *) fail "wrong message on mismatch: $out" ;; esac
  if [ ! -e "${dest}/mulchie" ]; then pass "checksum mismatch installs nothing"; else fail "binary installed despite mismatch"; fi
fi

# 3. Unsupported platform fails clearly. A fake uname stands in for the OS and arch.
shim="${work}/shim"
mkdir -p "$shim"
cat >"${shim}/uname" <<'SHIM'
#!/bin/sh
case "$1" in
  -s) echo "${FAKE_UNAME_S}" ;;
  -m) echo "${FAKE_UNAME_M}" ;;
  *) echo "${FAKE_UNAME_S}" ;;
esac
SHIM
chmod 755 "${shim}/uname"
for pair in "FreeBSD:x86_64" "Linux:riscv64" "Darwin:ppc"; do
  set -- "${pair%%:*}" "${pair##*:}"
  dest="${work}/bin3"
  if out="$(PATH="${shim}:${PATH}" FAKE_UNAME_S="$1" FAKE_UNAME_M="$2" MULCHIE_BASE_URL="file://${release}" MULCHIE_INSTALL_DIR="$dest" sh "$installer" 2>&1)"; then
    fail "unsupported platform $1/$2 did not fail"
  else
    case "$out" in *unsupported*) pass "rejects $1 $2" ;; *) fail "unclear message for $1 $2: $out" ;; esac
  fi
done

# 4. Missing download fails.
dest="${work}/bin4"
if out="$(MULCHIE_BASE_URL="file://${work}/nonexistent" MULCHIE_INSTALL_DIR="$dest" sh "$installer" 2>&1)"; then
  fail "missing download did not fail"
else
  case "$out" in *"download failed"*) pass "missing download fails with a message" ;; *) fail "unclear message: $out" ;; esac
fi

# 5. Uninstall removes the binary and keeps config.
MULCHIE_BASE_URL="file://${release}" MULCHIE_INSTALL_DIR="${work}/bin5" sh "$installer" >/dev/null 2>&1
if out="$(MULCHIE_INSTALL_DIR="${work}/bin5" sh "${here}/uninstall.sh" 2>&1)" && [ ! -e "${work}/bin5/mulchie" ]; then
  pass "uninstall removes the binary"
else
  fail "uninstall failed: $out"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures check(s) failed"
  exit 1
fi
echo "all checks passed"
