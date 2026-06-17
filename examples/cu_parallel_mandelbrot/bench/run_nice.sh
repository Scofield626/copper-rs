#!/usr/bin/env bash
# NICE-policy benchmark for cu_parallel_mandelbrot, under CPU contention.
#
# Niceness only changes scheduling when the CPU is contended, so every condition
# here runs against the SAME background hog: N `yes` burners pinned (taskset) to
# the same physical cores the rt pool pins to. The hog runs at default nice 0;
# only the rt pool's policy varies between conditions.
#
# Uses the matched bench/config_matched_*.ron files (regenerate them first for the
# current host via bench/gen_matched_config.py). The ONLY variable across conditions
# is the rt pool's scheduling policy:
#
#   Nbase  policy Fair       (config_matched_rt.ron)      pipeline contends evenly with hog
#   Nnice  policy Nice(-10)  (config_matched_nice.ron)    pipeline outranks hog  (needs RLIMIT_NICE)
#   Nhi    policy Nice(10)   (config_matched_nice10.ron)  pipeline yields to hog (sanity anchor)
#
# PRIVILEGE: Nice(-10) needs RLIMIT_NICE raised. Two ways to get it before running:
#   (a) in THIS shell:   sudo prlimit --pid $$ --nice=40:40   (no re-login; tmux-friendly)
#   (b) fresh PAM login:  via /etc/security/limits.d/99-copper-rt.conf (nice -20)
# NOTE: `ulimit -e` misreports 0 on some hosts even when the limit is actually raised, so
# the guard below probes the real capability (a negative setpriority) instead of reading
# ulimit. This way a missing privilege aborts cleanly rather than letting on_error:Strict
# hard-fail mid-run.
#
# Snapshots the live copperconfig.ron on entry and restores via trap (combined with the
# hog cleanup), so running the bench never mutates the shipped example config.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
EX="$ROOT/examples/cu_parallel_mandelbrot"
BIN="$ROOT/target/release/cu-parallel-mandelbrot"
CFG="$EX/copperconfig.ron"
OUT="$EX/bench/results_nice.csv"

# Default the hog to fully load the matched config's physical-core footprint.
detect_phys_cores() {
  local n=""
  if command -v lscpu >/dev/null; then
    n="$(lscpu -b -p=Core 2>/dev/null | grep -v '^#' | sort -u | wc -l)"
  fi
  [[ -n "$n" && "$n" -gt 0 ]] || n="$(( $(nproc) / 2 ))"
  [[ "$n" -gt 0 ]] || n=1
  echo "$n"
}
PHYS_CORES="$(detect_phys_cores)"
ROUNDS="${ROUNDS:-3}"
REPS="${REPS:-3}"
HOG_THREADS="${HOG_THREADS:-$PHYS_CORES}"
HOG_CORES="${HOG_CORES:-0-$((PHYS_CORES - 1))}"

CFG_BACKUP="$(mktemp --tmpdir copperconfig.ron.bench-backup.XXXX)"
cp -- "$CFG" "$CFG_BACKUP"
restore_cfg() { cp -- "$CFG_BACKUP" "$CFG"; rm -f -- "$CFG_BACKUP"; }

# --- privilege guard: actually probe whether a child can set nice -10 (inherited RLIMIT_NICE) ---
# Forks a child that sets only its OWN nice, so the probe cannot perturb this shell or the hog.
if ! python3 -c "import os; os.setpriority(os.PRIO_PROCESS,0,-10)" 2>/dev/null; then
  echo "ERROR: cannot set nice -10 (RLIMIT_NICE too low for Nice(-10), on_error:Strict would hard-fail)." >&2
  echo "       Raise it first, e.g.:  sudo prlimit --pid \$\$ --nice=40:40   then re-run this script." >&2
  exit 1
fi

HOG_PIDS=()
start_hog() {
  HOG_PIDS=()
  for _ in $(seq 1 "$HOG_THREADS"); do
    taskset -c "$HOG_CORES" yes >/dev/null 2>&1 &
    HOG_PIDS+=("$!")
  done
  echo "# hog: $HOG_THREADS x yes pinned to cores $HOG_CORES (pids ${HOG_PIDS[*]})" >&2
}
stop_hog() {
  [[ ${#HOG_PIDS[@]} -eq 0 ]] && return 0
  kill "${HOG_PIDS[@]}" 2>/dev/null || true
  wait "${HOG_PIDS[@]}" 2>/dev/null || true
  HOG_PIDS=()
}
cleanup() { stop_hog; restore_cfg; }
trap cleanup EXIT

build() { cargo build -q -p cu-parallel-mandelbrot --bin cu-parallel-mandelbrot --release --features "$1"; }

run_one() {
  local cond="$1" line es fhz dig fe
  line="$("$BIN" </dev/null 2>/dev/null | grep 'BENCH summary')"
  es="$(sed -n 's/.*elapsed_s=\([0-9.]*\).*/\1/p' <<<"$line")"
  fhz="$(sed -n 's/.*frame_hz=\([0-9.]*\).*/\1/p' <<<"$line")"
  dig="$(sed -n 's/.*last_frame_digest=\(0x[0-9a-f]*\).*/\1/p' <<<"$line")"
  fe="$(sed -n 's/.*frames_emitted=\([0-9]*\).*/\1/p' <<<"$line")"
  echo "$cond,$es,$fhz,$fe,$dig" | tee -a "$OUT"
}

prep_Nbase() { cp "$EX/bench/config_matched_rt.ron"     "$CFG"; build "cu29/parallel-rt cu29/rt-scheduling"; }
prep_Nnice() { cp "$EX/bench/config_matched_nice.ron"   "$CFG"; build "cu29/parallel-rt cu29/rt-scheduling"; }
prep_Nhi()   { cp "$EX/bench/config_matched_nice10.ron" "$CFG"; build "cu29/parallel-rt cu29/rt-scheduling"; }

cd "$ROOT"
echo "condition,elapsed_s,frame_hz,frames_emitted,digest" > "$OUT"
echo "# matched 13-band graph under hog; ROUNDS=$ROUNDS REPS=$REPS HOG=$HOG_THREADS@$HOG_CORES ($(nproc) CPUs, nice -10 capable)" >&2

start_hog
for r in $(seq 1 "$ROUNDS"); do
  for cond in Nbase Nnice Nhi; do
    echo ">>> round $r $cond (build)" >&2
    prep_$cond
    if [[ "$r" == "1" ]]; then "$BIN" </dev/null >/dev/null 2>&1 || true; fi  # warm-up
    for i in $(seq 1 "$REPS"); do run_one "$cond"; done
  done
done
stop_hog

echo "# done -> $OUT" >&2
