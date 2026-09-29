#!/usr/bin/env bash
# Regenerates the VAD fixtures: `nix develop -c bash src-tauri/tests/fixtures/vad/gen.sh`
# <name>.truth holds one `start_ms end_ms` line per utterance.
set -euo pipefail
out="$(cd "$(dirname "$0")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
SR=22050

say() { # name voice text
  espeak-ng -v "$2" -s 160 -w "$tmp/$1.raw.wav" "$3"
  sox -R -D "$tmp/$1.raw.wav" -r $SR -c 1 -b 16 "$tmp/$1.wav" \
    silence 1 0.001 -60d reverse silence 1 0.001 -60d reverse norm -6
}

build() { # name gap_secs utt gap_secs utt ... trailing_gap_secs
  local name="$1"; shift
  local parts=() pos=0 truth="" g u n
  while (($#)); do
    g="$tmp/gap_$1.wav"
    sox -R -D -n -r $SR -c 1 -b 16 "$g" trim 0 "$1"
    parts+=("$g"); pos=$((pos + $(soxi -s "$g"))); shift
    (($#)) || break
    u="$tmp/$1.wav"; n=$(soxi -s "$u"); parts+=("$u"); shift
    truth+="$((pos * 1000 / SR)) $(((pos + n) * 1000 / SR))"$'\n'
    pos=$((pos + n))
  done
  printf %s "$truth" > "$out/$name.truth"
  sox -R -D "${parts[@]}" "$tmp/mix_$name.wav"
  for rate in 16000 44100 48000; do
    sox -R -D "$tmp/mix_$name.wav" -b 16 "$out/${name}_$rate.wav" rate -v $rate
  done
}

say u1 en-us "Can you walk me through how you would design a rate limiter"
say u2 en-us "Assume it has to work across several data centers"
say u3 en-us "What happens when one region loses connectivity"
say u4 en-gb+f3 "I would start with a token bucket per client and then shard the counters"
say long en-us "Tell me about a project where you had to coordinate with several other teams under a tight deadline and describe what you personally did to keep everyone aligned and what you would change if you had to do it again today"

build pauses 1.0 u1 0.5 u2 1.5 u3 3.0 u4 2.0
build long 1.0 long 2.0
