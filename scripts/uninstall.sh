#!/bin/sh
# Remove the mulchie binary installed by install.sh.
#
# Environment:
#   MULCHIE_INSTALL_DIR  Where the binary was installed. Default: $HOME/.local/bin.
#
# Your configuration in ~/.config/mulchie is not touched.

set -eu

main() {
  install_dir="${MULCHIE_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}"
  target="${install_dir}/mulchie"

  if [ -e "$target" ] || [ -L "$target" ]; then
    rm -f "$target"
    echo "Removed ${target}"
  else
    echo "No mulchie binary at ${target}"
  fi

  echo ""
  echo "Left in place: ${HOME}/.config/mulchie (your settings)."
  echo "Delete it yourself if you want it gone: rm -r \"\$HOME/.config/mulchie\""
  echo "If you installed a background service, remove it first with: mulchie service uninstall"
  echo "Installed with Homebrew instead? Use: brew uninstall mulchie-cli   (or --cask mulchie)"
}

main "$@"
