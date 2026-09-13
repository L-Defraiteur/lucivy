#!/bin/bash
# A poor man's sampling profiler for when perf_event_open is refused
# (`kernel.perf_event_paranoid` 2): gdb runs the program, this script sends it
# SIGALRM every INTERVAL seconds, gdb catches each one (the program never sees
# it), dumps every thread's backtrace and continues. Aggregate with gdb_top.py.
# ptrace_scope 1 is enough: the program is gdb's child.
#
#   benches/gdb_sample.sh <samples.txt> [interval=0.25] [max=400] -- <binary> <args...>
#
# The program's stdout/stderr go to <samples.txt>.gdb.
OUT="$1"; shift
INTERVAL=0.25; MAX=400
if [ "$1" != "--" ]; then INTERVAL="$1"; shift; fi
if [ "$1" != "--" ]; then MAX="$1"; shift; fi
[ "$1" = "--" ] && shift
BIN="$1"
ARGS=(-q -batch -ex "set pagination off" -ex "set confirm off" -ex "set print thread-events off"
      -ex "set width 0" -ex "set debuginfod enabled off" -ex "handle SIGALRM stop print nopass"
      -ex "set print frame-arguments none" -ex "set print frame-info short-location"
      -ex "set backtrace limit 24" -ex "set logging file $OUT" -ex "set logging overwrite on" -ex "set logging redirect on"
      -ex "set logging enabled on" -ex "run")
for ((i = 0; i < MAX; i++)); do
  ARGS+=(-ex "echo === sample\n" -ex "python print('T=%.6f' % __import__('time').time())" -ex "thread apply all bt" -ex "continue")
done
gdb "${ARGS[@]}" --args "$@" > "$OUT.gdb" 2>&1 &
GDBPID=$!
# The inferior is gdb's child; gdb may spend seconds loading symbols first.
PID=""
for ((i = 0; i < 300; i++)); do
  PID=$(pgrep -o -P $GDBPID)
  [ -n "$PID" ] && break
  sleep 0.1
done
echo "sampling pid $PID under gdb $GDBPID" >&2
while kill -0 "$PID" 2>/dev/null; do kill -ALRM "$PID" 2>/dev/null; sleep "$INTERVAL"; done
wait $GDBPID
