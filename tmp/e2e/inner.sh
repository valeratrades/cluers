#!/usr/bin/env bash
# Runs inside the private dbus session (see run.sh).
set -euo pipefail
E=/home/v/s/other/cluers/tmp/e2e
R=$E/run
pids=()
exec {xfd}>"$R/inner.trace"; BASH_XTRACEFD=$xfd; set -x
trap 'kill "${pids[@]}" 2>/dev/null; wait' EXIT

if [[ -z ${NO_KEYRING:-} ]]; then
  printf e2e | gnome-keyring-daemon --daemonize --unlock --components=secrets > "$R/keyring.env"
  . <(sed 's/^/export /' "$R/keyring.env")
fi

printf 'output HEADLESS-1 resolution 1920x1080 position 0 0\ndefault_border pixel 1\nfocus_on_window_activation none\n' > "$R/sway.conf"
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=pixman sway -d -c "$R/sway.conf" > "$R/sway.log" 2>&1 & pids+=($!)
for _ in $(seq 50); do compgen -G "$R/rt/wayland-*" > /dev/null && compgen -G "$R/rt/sway-ipc.*" > /dev/null && break; sleep 0.1; done
export WAYLAND_DISPLAY=$(basename "$(compgen -G "$R/rt/wayland-*" | grep -v lock | head -1)") GDK_BACKEND=wayland
export SWAYSOCK=$(compgen -G "$R/rt/sway-ipc.*")

export NO_COLOR=1 RUST_LOG=debug,hyper=info,hyper_util=info,reqwest=info,h2=info,rustls=info,tao=info,wry=info,tauri=info,zbus=info,tracing=info
export PULSE_SERVER=unix:/run/user/1000/pulse/native PULSE_RUNTIME_PATH=/run/user/1000/pulse
WKD=$(dirname "$(readlink -f "$(command -v WebKitWebDriver 2>/dev/null || echo /nix/store/nrmz5y0i49qmbj40na6piya3kzcc3g36-webkitgtk-2.52.3+abi=4.1/bin/WebKitWebDriver)")")/WebKitWebDriver
printf '#!/bin/sh\nexec %s "$@" >> %s 2>&1\n' "$E/target/debug/pluely" "$R/app.log" > "$R/pluely"; chmod +x "$R/pluely"  # tauri-driver drops the app stdout, where tracing goes
$E/cargo/bin/tauri-driver --port 14444 --native-port 14445 --native-driver "$WKD" > "$R/driver.log" 2>&1 & pids+=($!)
sleep 1
$E/py/bin/python $E/test.py "$@"
