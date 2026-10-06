#!/usr/bin/env bash
# Headless e2e: private HOME/dbus/sway/keyring, null sinks on the real Pulse server, mock STT+LLM, WebDriver.
# Build: CARGO_TARGET_DIR=$PWD/tmp/e2e/target nix develop -c npx tauri build --debug --no-bundle
# Tools (once): nix develop -c cargo install tauri-driver --locked --root tmp/e2e/cargo
#   nix build --impure --expr '(builtins.getFlake "nixpkgs").legacyPackages.x86_64-linux.python3.withPackages (p: [p.selenium])' -o tmp/e2e/py
# Run outside nix develop (the nested sway never opens its socket inside it): tmp/e2e/run.sh [scenario ...]
# Scenarios: see SCEN in test.py. Keychain-free run: NO_KEYRING=1 tmp/e2e/run.sh nokeyring. Artifacts in tmp/e2e/run/: app.log, mock.jsonl, NN-*.png
set -euo pipefail
E=/home/v/s/other/cluers/tmp/e2e
R=$E/run
rm -rf "$R"; mkdir -p "$R/home/.config" "$R/home/.local/share"; mkdir -m700 "$R/rt"
export PULSE_SERVER=unix:/run/user/1000/pulse/native PULSE_RUNTIME_PATH=/run/user/1000/pulse

def_sink=$(pactl get-default-sink); def_src=$(pactl get-default-source)
mods=()
cleanup() {
  set +e
  [[ -n ${mock:-} ]] && kill "$mock"
  for m in "${mods[@]}"; do pactl unload-module "$m"; done
  [[ $(pactl get-default-sink) == "$def_sink" && $(pactl get-default-source) == "$def_src" ]] \
    || echo "!!! default sink/source changed: $(pactl get-default-sink) $(pactl get-default-source)" >&2
  pactl list short modules | grep cluers_e2e && echo "!!! leftover cluers_e2e modules" >&2
}
trap cleanup EXIT
trap "exit 130" INT TERM
for s in cluers_e2e_sys cluers_e2e_mic; do
  mods=("$(pactl load-module module-null-sink sink_name=$s sink_properties=device.description=$s)" "${mods[@]}")
done
# a real (non-monitor) source, so it shows up in the app's input device list
mods=("$(pactl load-module module-remap-source master=cluers_e2e_mic.monitor source_name=cluers_e2e_micsrc source_properties=device.description=cluers_e2e_micsrc)" "${mods[@]}")
[[ $(pactl get-default-sink) == "$def_sink" && $(pactl get-default-source) == "$def_src" ]] \
  || { echo "loading null sinks moved the default device, aborting" >&2; exit 1; }

LOG=$R/mock.jsonl python3 $E/mock.py 18765 > "$R/mock.err" 2>&1 & mock=$!

env -u WAYLAND_DISPLAY -u DISPLAY -u SWAYSOCK -u DBUS_SESSION_BUS_ADDRESS -u I3SOCK \
  HOME=$R/home XDG_RUNTIME_DIR=$R/rt XDG_CONFIG_HOME=$R/home/.config XDG_DATA_HOME=$R/home/.local/share \
  XDG_CACHE_HOME=$R/home/.cache XDG_STATE_HOME=$R/home/.local/state \
  dbus-run-session -- bash $E/inner.sh "$@"
