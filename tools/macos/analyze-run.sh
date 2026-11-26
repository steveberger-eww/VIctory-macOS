#!/bin/sh
# Summarises a run that tools/macos/record.sh recorded, by default the
# latest, in about 60 lines: how it ended, frame rate and memory over time,
# and each kind of warning and error with its count.
#   tools/macos/analyze-run.sh [target/runs/<run>]
cd "$(dirname "$0")/../.."
RUN="${1:-$(ls -td target/runs/*/ | head -n 1)}"
LOG="$RUN/game.log"
[ -f "$LOG" ] || { echo "no game.log in $RUN" >&2; exit 1; }
# Colour codes are dropped, in case the log has them.
plain() { sed 's/\x1b\[[0-9;]*m//g' "$LOG"; }

echo "== $RUN ($(cat "$RUN/args" 2> /dev/null))"
[ -s "$RUN/env" ] && echo "env: $(tr '\n' ' ' < "$RUN/env")"
grep -q "masked stores .* patched" "$RUN/game.log" 2> /dev/null || echo "WARNING: masked stores not patched (VICTORY_PATCH_MASKED=all missing): copies will be whole"
echo "exit: $(cat "$RUN/exit" 2> /dev/null || echo 'none: still running, or the recorder was killed')"
echo "log: $(plain | grep -m 1 -o '^[0-9T:.-]*Z') to $(plain | grep -o '^[0-9T:.-]*Z' | tail -n 1), $(wc -l < "$LOG" | tr -d ' ') lines"
echo "crash reports: $(ls "$RUN" | grep -c '\.ips$') ($(ls "$RUN" | grep '\.ips$' | sed 's/-20[0-9-]*.*//' | sort | uniq -c | tr -s ' ' | tr '\n' ',' ))"

echo "== frames (every 10th report, then the last)"
plain | grep ' fps, ' | awk 'NR % 10 == 1 { print } { last = $0 } END { print last }' \
  | sed -E 's/^[0-9-]+T([0-9:]+)\.[0-9]*Z.* ([0-9.]+ fps), ([0-9.]+) ms\/frame.*copies ([0-9.]+ ms [0-9.]+ [MKG]B).* ([0-9.]+) on the GPU.*/\1 \2 \3 ms (copies \4, GPU \5 ms)/' \
  | cut -c1-110
echo "== host textures (first, peak, last)"
plain | grep 'host textures: ' | sed -E 's/^[0-9-]+T([0-9:]+).*host textures: ([0-9]+) \(([0-9]+) MB.*/\1 \2 \3/' \
  | awk 'NR == 1 { print "first", $0 } $3 > m { m = $3; p = $0 } { l = $0 } END { print "peak ", p; print "last ", l }'

# Only a diagnostics build (VICTORY_DIAGNOSTICS=1) logs these.
plain | grep 'host copies of title memory' | sed -E 's/^[0-9-]+T([0-9:]+).*(host copies of title memory: .*)/\1 \2/' \
  | awk 'NR == 1 { print "== host copies of title memory (first, last)"; print } { l = $0 } END { if (NR) print l }' | cut -c1-140
plain | grep 'host copies by kind' | sed -E 's/^[0-9-]+T([0-9:]+).*host copies by kind: /\1 /' \
  | awk 'NR % 20 == 1 { print } { l = $0 } END { if (NR) print l }' | cut -c1-170
plain | grep 'placed textures: ' | sed -E 's/^[0-9-]+T([0-9:]+).*(placed textures: )/\1 \2/' \
  | awk 'NR % 20 == 1 { print } { l = $0 } END { if (NR) print l }' | cut -c1-170

if [ -f "$RUN/memory.log" ]; then
  echo "== memory in MB (first, peak wired, lowest free, last)"
  awk '{ for (i = 2; i <= NF; i++) { split($i, kv, "="); v[kv[1]] = kv[2] } }
       NR == 1 { print "first", $0 }
       v["wired"] > w { w = v["wired"]; pw = $0 }
       NR == 1 || v["free"] < f { f = v["free"]; pf = $0 }
       { l = $0 }
       END { print "wired", pw; print "free ", pf; print "last ", l }' "$RUN/memory.log"
fi

echo "== warnings and errors by kind"
# The key drops the time and every field=value, so one kind is one line.
plain | grep -E ' (WARN|ERROR) |\]\[ERROR\]|panic|rosetta error' \
  | sed -E 's/^[0-9T:.-]+Z +//; s/ [a-z_]+=("[^"]*"|\{[^}]*\}|[^ ]*)//g; s/UserInfo=.*//; s/dropped code at .*/dropped (by reason below)/' \
  | cut -c1-120 | sort | uniq -c | sort -rn | head -n 25
echo "== shaders that do not translate, by reason"
plain | grep 'does not translate' | sed -E 's/.*code at 0x[0-9a-f]+: //; s/ cached=.*//' | sort | uniq -c | sort -rn
echo "== last lines"
plain | tail -n 5 | cut -c1-200
