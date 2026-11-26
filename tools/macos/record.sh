#!/bin/sh
# Starts the game detached from this terminal and records the run until it
# exits or crashes: the game's log, the Mac's memory every 5 s, the exit
# code and the crash reports written meanwhile. Nothing is printed while it
# runs, so an agent watching it pays for nothing; read the run afterwards
# with tools/macos/analyze-run.sh. Arguments and environment go to
# victory.sh, for example:
#   VICTORY_PATCH_MASKED=all VICTORY_FAULT_TRACKING=1 tools/macos/record.sh \
#     run --execute <path to the game's executable>
set -e
cd "$(dirname "$0")/../.."
RUN="$PWD/target/runs/$(date +%Y%m%d-%H%M%S)-$$"
mkdir -p "$RUN"
echo "$*" > "$RUN/args"
env | grep "^VICTORY_" | sort > "$RUN/env"
export VICTORY_LOG="${VICTORY_LOG:-info}"
# setsid puts the run in its own session, so closing the terminal or
# ending the agent that started it does not stop the game.
perl -MPOSIX -e 'POSIX::setsid(); exec @ARGV' sh -c '
  RUN="$1"; shift
  touch "$RUN/started"
  NO_COLOR=1 tools/macos/victory.sh "$@" > "$RUN/game.log" 2>&1 &
  game=$!
  # One line per sample: time, wired, free and compressed memory in MB, and
  # the resident size of victory.exe. Wired holds Metal'\''s allocations,
  # which the resident size leaves out.
  while kill -0 $game 2> /dev/null; do
    vm_stat | awk -v t="$(date -u +%H:%M:%S)" -v rss="$(ps -o rss= -p "$(pgrep -f "release/victory\.exe" | head -n 1)" 2> /dev/null)" "
      /page size of/ { p = \$8 }
      /Pages wired down/ { w = \$4 } /Pages free/ { f = \$3 } /occupied by compressor/ { c = \$5 }
      END { printf \"%s wired=%d free=%d compressed=%d rss=%d\\n\", t, w*p/1048576, f*p/1048576, c*p/1048576, rss/1024 }
    " >> "$RUN/memory.log"
    sleep 5
  done
  wait $game; echo $? > "$RUN/exit"
  find ~/Library/Logs/DiagnosticReports -maxdepth 1 -newer "$RUN/started" \
    \( -name "MTLCompilerService*" -o -name "wine*" -o -name "victory*" \) -exec cp {} "$RUN/" \; 2> /dev/null
' sh "$RUN" "$@" < /dev/null > "$RUN/record.log" 2>&1 &
echo "recording in $RUN"
