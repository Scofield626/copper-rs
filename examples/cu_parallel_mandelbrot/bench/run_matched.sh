#!/usr/bin/env bash
# Apples-to-apples affinity test on a machine-matched pipeline.
#
# Uses bench/config_matched_*.ron, which are sized so src + N bands + frames +
# image_drain = N+3 worker lanes = physical core count, giving an exact 1-lane-per-core
# pinning. Regenerate them for the current host first:
#   bench/gen_matched_config.py        # auto-detects physical cores
# Same workload for both conditions; only the rt pool differs.
#
#   Bm  parallel-rt, no thread_pools            (OS-migrated across all CPUs)
#   Cm  parallel-rt + rt pool affinity:[0..N-1] (pinned 1 lane per physical core)
#
# Snapshots the live copperconfig.ron on entry and restores via trap, so running
# the bench never mutates the shipped example config (even on abort).
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
EX="$ROOT/examples/cu_parallel_mandelbrot"
BIN="$ROOT/target/release/cu-parallel-mandelbrot"
CFG="$EX/copperconfig.ron"
OUT="$EX/bench/results_matched.csv"

CFG_BACKUP="$(mktemp --tmpdir copperconfig.ron.bench-backup.XXXX)"
cp -- "$CFG" "$CFG_BACKUP"
restore_cfg() { cp -- "$CFG_BACKUP" "$CFG"; rm -f -- "$CFG_BACKUP"; }
trap restore_cfg EXIT

ROUNDS="${ROUNDS:-3}"
REPS="${REPS:-3}"

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

prep_Bm() { cp "$EX/bench/config_matched_base.ron" "$CFG"; build "cu29/parallel-rt"; }
prep_Cm() { cp "$EX/bench/config_matched_rt.ron"   "$CFG"; build "cu29/parallel-rt cu29/rt-scheduling"; }

cd "$ROOT"
echo "condition,elapsed_s,frame_hz,frames_emitted,digest" > "$OUT"
echo "# matched 13-band graph; ROUNDS=$ROUNDS REPS=$REPS ($(nproc) CPUs)" >&2

for r in $(seq 1 "$ROUNDS"); do
  for cond in Bm Cm; do
    echo ">>> round $r $cond (build)" >&2
    prep_$cond
    if [[ "$r" == "1" ]]; then "$BIN" </dev/null >/dev/null 2>&1 || true; fi  # warm-up
    for i in $(seq 1 "$REPS"); do run_one "$cond"; done
  done
done

echo "# done -> $OUT" >&2
