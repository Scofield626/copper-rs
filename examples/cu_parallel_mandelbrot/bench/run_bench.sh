#!/usr/bin/env bash
# Thread-pool benchmark driver for cu_parallel_mandelbrot.
#
# Compares three conditions on the deterministic, compute-bound parallel-rt graph:
#   A serial        - no parallel-rt (one CopperList at a time)        [reference floor]
#   B parallel      - parallel-rt, no thread_pools declared            [pre-PR behavior]
#   C parallel_aff  - parallel-rt + rt pool pinned to phys cores 0-15  [this PR]
#
# Metrics per run come from the BENCH summary line the binary prints to stdout:
#   elapsed_s  (app.run() only, low-noise)   frame_hz   last_frame_digest (determinism gate)
#
# Config is baked at compile time, so each condition is a separate build. We run
# ROUNDS passes over {A,B,C} to spread each condition's samples across wall-clock
# time (guards against transient cloud-host noise). Results: bench/results_raw.csv
#
# The driver swaps in bench configs by overwriting the example's copperconfig.ron, so
# it snapshots the live config on entry and restores it via trap. Running the bench
# never mutates the shipped example config, even on abort.
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
EX="$ROOT/examples/cu_parallel_mandelbrot"
BIN="$ROOT/target/release/cu-parallel-mandelbrot"
CFG="$EX/copperconfig.ron"
OUT="$EX/bench/results_raw.csv"

CFG_BACKUP="$(mktemp --tmpdir copperconfig.ron.bench-backup.XXXX)"
cp -- "$CFG" "$CFG_BACKUP"
restore_cfg() { cp -- "$CFG_BACKUP" "$CFG"; rm -f -- "$CFG_BACKUP"; }
trap restore_cfg EXIT

ROUNDS="${ROUNDS:-2}"
# Per-condition reps per round. Serial is a stable but ~285s/run floor, so it
# gets few reps; B/C are the ~26s/run comparison of interest and get more.
REPS_A="${REPS_A:-1}"
REPS_B="${REPS_B:-4}"
REPS_C="${REPS_C:-4}"

build() { # $1=features (may be empty)
  if [[ -n "$1" ]]; then
    cargo build -q -p cu-parallel-mandelbrot --bin cu-parallel-mandelbrot --release --features "$1"
  else
    cargo build -q -p cu-parallel-mandelbrot --bin cu-parallel-mandelbrot --release
  fi
}

run_one() { # $1=condition label
  local cond="$1" line es fhz dig fe
  line="$("$BIN" </dev/null 2>/dev/null | grep 'BENCH summary')"
  es="$(sed -n 's/.*elapsed_s=\([0-9.]*\).*/\1/p' <<<"$line")"
  fhz="$(sed -n 's/.*frame_hz=\([0-9.]*\).*/\1/p' <<<"$line")"
  dig="$(sed -n 's/.*last_frame_digest=\(0x[0-9a-f]*\).*/\1/p' <<<"$line")"
  fe="$(sed -n 's/.*frames_emitted=\([0-9]*\).*/\1/p' <<<"$line")"
  echo "$cond,$es,$fhz,$fe,$dig" | tee -a "$OUT"
}

prep_A() { cp "$EX/bench/config_base.ron" "$CFG"; build ""; }
prep_B() { cp "$EX/bench/config_base.ron" "$CFG"; build "cu29/parallel-rt"; }
prep_C() { cp "$EX/bench/config_rt.ron"   "$CFG"; build "cu29/parallel-rt cu29/rt-scheduling"; }

cd "$ROOT"
echo "condition,elapsed_s,frame_hz,frames_emitted,digest" > "$OUT"
echo "# ROUNDS=$ROUNDS REPS_A=$REPS_A REPS_B=$REPS_B REPS_C=$REPS_C  ($(nproc) CPUs)" >&2

for r in $(seq 1 "$ROUNDS"); do
  for cond in A B C; do
    echo ">>> round $r condition $cond (build)" >&2
    prep_$cond
    reps_var="REPS_$cond"; reps="${!reps_var}"
    # discard a warm-up on first touch of each fast condition (skip slow serial)
    if [[ "$r" == "1" && "$cond" != "A" ]]; then "$BIN" </dev/null >/dev/null 2>&1 || true; fi
    for i in $(seq 1 "$reps"); do
      echo ">>> round $r condition $cond rep $i" >&2
      run_one "$cond"
    done
  done
done

echo "# done -> $OUT" >&2
