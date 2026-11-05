#!/usr/bin/env bash
# Install the bridge and the plugin from this checkout into your home
# directory, for development without building a package.
#
#   scripts/dev-install.sh            build + install
#   scripts/dev-install.sh --link     symlink the plugin dir (edits apply on reload)
#
# The bridge goes to ~/.local/bin; the plugin to Noctalia's local plugin
# source (~/.local/share/noctalia/plugins/appmenu), which Noctalia scans by
# itself. Enable it afterwards with:
#   noctalia msg plugins enable goglioandrea/appmenu
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
link=0
[[ "${1:-}" == "--link" ]] && link=1

bin_dir="${HOME}/.local/bin"
data_home="${XDG_DATA_HOME:-${HOME}/.local/share}"
plugin_dir="${data_home}/noctalia/plugins/appmenu"

echo "==> building the bridge"
(cd "${repo}/bridge" && cargo build --release --locked --bin noctalia-appmenu-bridge)
install -Dm755 "${repo}/bridge/target/release/noctalia-appmenu-bridge" "${bin_dir}/noctalia-appmenu-bridge"

echo "==> installing the plugin to ${plugin_dir}"
mkdir -p "$(dirname "${plugin_dir}")"
if [[ -L "${plugin_dir}" ]]; then
  rm -- "${plugin_dir}"
elif [[ -d "${plugin_dir}" ]]; then
  rm -r -- "${plugin_dir}"
fi
if (( link )); then
  ln -s "${repo}/plugins/appmenu" "${plugin_dir}"
else
  cp -r "${repo}/plugins/appmenu" "${plugin_dir}"
fi

cat <<EOF

Done. Next steps:
  noctalia msg plugins enable goglioandrea/appmenu
  then add the "AppMenu" widget to your bar (Settings -> Bar).

If Noctalia's PATH does not include ${bin_dir}, set the plugin's
"Bridge command" setting to: ${bin_dir}/noctalia-appmenu-bridge
Bridge log: \$XDG_RUNTIME_DIR/noctalia-appmenu-bridge.log
EOF
