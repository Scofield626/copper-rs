# cu-parallel-mandelbrot — thread-pool benchmark

Three benchmarks that quantify the `runtime.thread_pools` feature
(`affinity` / `policy` / `rt-scheduling`) on the deterministic, compute-bound
`parallel-rt` mandelbrot graph. Each is one `just` recipe.

| Recipe | What it measures | CSV |
|---|---|---|
| `just bench` | Throughput floor (serial), `parallel-rt` baseline, and `parallel-rt` + pinned `rt` pool, on the 32-band default graph. | `results_raw.csv` |
| `just bench-matched` | Pinning the `rt` pool with one worker lane per physical core (apples-to-apples vs the unpinned baseline). | `results_matched.csv` |
| `just bench-nice` | `Nice(-10)` / Fair / `Nice(10)` rt-pool policies under a CPU hog, to isolate nice's effect. | `results_nice.csv` |

The committed [results.md](results.md) is the full writeup with medians, ranges,
and interpretation from the documented reference host (16 physical cores).

## Reproducibility on any host (the matched bench)

The matched configs are *machine-derived*, not hardcoded. The generator
[gen_matched_config.py](gen_matched_config.py) auto-detects the host's physical
core count (`lscpu -b -p=Core`) and emits the four matched variants with:

- `bands = cores − 3` (3 non-band lanes: `src`, `frames`, `image_drain`)
- `band_iters = ceil(max_iter / bands)` (cumulative iters ≥ `max_iter`)
- `affinity = [0 .. cores − 1]`

The numerical output (and the determinism digest) is invariant to `cores` — a
16-core run and a 32-core run both fully iterate every pixel. `just bench-matched`
and `just bench-nice` regenerate before running; pass `--cores N` explicitly to
override (`python3 bench/gen_matched_config.py --cores 8`).

## Privilege setup for `just bench-nice`

`Nice(-10)` (favoring the rt pool over the hog) needs `RLIMIT_NICE` raised. With
the pool's `on_error: Strict`, missing privilege hard-fails at startup; the script
probes the real capability first and aborts cleanly with the fix-it command.

Two equivalent ways to raise it:

- **In your current shell** (tmux-friendly, no re-login):
  ```bash
  sudo prlimit --pid $$ --nice=40:40
  ```
- **Persistently for your user** (via `/etc/security/limits.d/`, takes effect on
  next PAM login):
  ```
  <user>  -  nice    -20
  <user>  -  rtprio  99
  ```

Heads-up: `ulimit -e` can misreport `0` on some hosts even when the limit *is*
raised. The bench script doesn't trust that readout — it probes with a real
negative `setpriority` call instead.

## Determinism gate

Every condition prints `last_frame_digest = 0x…`. All runs on a given workload
must produce the same digest; if pinning or scheduling perturbs numerical output,
that's a correctness regression, not a tuning question.

## How the harness works

- Each condition is a separate `cargo build --release` because the config is
  baked in by `#[copper_runtime(config = …)]` at compile time.
- The driver swaps in bench configs by overwriting the example's `copperconfig.ron`.
  It snapshots the live file on entry and restores it via `trap`, so running a
  benchmark never mutates the shipped example config — even on abort.
- Metrics come from the `BENCH summary` line `src/lib.rs` prints to stdout, so
  the harness doesn't have to parse the unified log to capture throughput and
  the digest.

## Hardware caveat

`results.md`'s numbers are from one host (16-physical-core / 32-logical Xeon).
Re-running on different hardware will produce different absolute numbers; what
should hold across hosts is the *shape* of the result — `parallel-rt` dominates
serial, pinning costs ~nothing at matched depth and tightens variance, and
`Nice(-10)` recovers throughput under contention. The CSVs are `.gitignore`d
because committed raw data goes stale; the tables in `results.md` are the
record of record.
