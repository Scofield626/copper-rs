# Thread-pool benchmark — cu_parallel_mandelbrot

Quantifies how the `runtime.thread_pools` feature (PR `feat/thread-pools-affinity-rt`)
affects the deterministic, compute-bound `parallel-rt` graph in this example.

## Environment

- Host: AWS, Intel Xeon Platinum 8175M @ 2.50 GHz, **16 physical cores × 2 HT = 32 logical CPUs**, single socket, virtualized.
- No `isolcpus`, no RT priority for the user (`ulimit -r` = 0), opaque turbo/governor → treat absolute Hz as relative only.
- Workload: 1920×1080, `stripe_rows=64` → 17 stripes/frame × 384 frames = **6,528 CopperLists**, each through **32 compute bands** (`band_iters=16`, `max_iter=512`). Stripe payloads are handle-backed.
- Build: `--release`. Console monitor removed from the bench config (pure overhead, removed consistently across all conditions). `mmap-fsync` off throughout.
- Metric source: the binary's `BENCH summary` line — `elapsed_s` is `app.run()` only (excludes process spawn / log teardown, lower-noise than wall-clock).

## Conditions

Config is baked at compile time, so each condition is a separate build. 2 rounds, interleaved.

| # | Condition | Build features | `rt` pool |
|---|---|---|---|
| A | serial floor | *(none)* | — |
| B | parallel-rt baseline (pre-PR behavior) | `parallel-rt` | none declared (workers float across all 32 CPUs, Fair) |
| C | parallel-rt + affinity | `parallel-rt` + `rt-scheduling` | `id:"rt"`, `affinity:[0..15]` (16 physical cores), `on_error:Strict` |

## Results

| Condition | n | elapsed_s median | min | max | range | frame_hz median | vs A | vs B |
|---|---|---|---|---|---|---|---|---|
| A serial | 2 | **285.32** | 285.29 | 285.34 | 0.05 | 1.35 | 1.00× | — |
| B parallel | 8 | **26.52** | 24.94 | 29.57 | 4.63 | 14.48 | **10.76×** | baseline |
| C parallel+affinity | 8 | **34.15** | 33.00 | 35.58 | 2.57 | 11.25 | 8.35× | **+28.8% slower** |

**Determinism gate: PASS.** All 18 runs (A, B, C) produced the identical
`last_frame_digest = 0xe738bfff449bb8bd`. Pinning + the `rt-scheduling` path do **not**
perturb numerical output — the core correctness property the PR must preserve holds.

## Interpretation

1. **`parallel-rt` is the dominant win here: ~10.8× over serial.** This graph is the
   ideal shape for it (a long, even CPU pipeline with many CopperLists in flight).

2. **Condition C (pin to 16 physical cores) regressed throughput ~29% — but this is a
   CPU-budget artifact, not evidence that pinning is bad.** `affinity:[0..15]` confines all
   ~34 stage workers to 16 logical CPUs, leaving CPUs 16–31 idle and oversubscribing 0–15
   roughly 2×. The baseline B is free to use all 32 logical CPUs. On this workload
   hyper-threading still scales positively, so halving the usable CPU budget costs ~29%.
   **C vs B is not an apples-to-apples test of pinning** — it conflates "pinned" with "half
   the CPUs". The clean equal-budget test is `affinity:[0..31]` (see follow-up).

3. **Pinning roughly halved run-to-run variance.** Spread fell from 4.63 s (B, ~17% of
   median) to 2.57 s (C, ~8%). Even while throughput dropped, the pinned run was markedly
   more *predictable* — the latency-determinism angle the affinity/RT feature targets. On a
   real robot you trade a little throughput for tighter, more bounded timing, which is
   usually the point of pinning a control loop.

## Matched-depth follow-up (apples-to-apples)

The A/B/C run above is confounded: `parallel-rt` spawns **one worker thread per graph
stage**, not per core. The default graph has 32 bands → `src + 32 bands + frames +
image_drain` = **35 worker lanes** (measured: 37 OS threads incl. main + logger) on a
32-CPU box. So condition C didn't test "pinning"; it crammed 35 lanes onto 16 CPUs.

The fix is to size pipeline *depth* to the machine. Trimming to **13 bands** gives
`src + 13 bands + frames + image_drain` = **16 worker lanes = the 16 physical cores**
(measured: 18 OS threads), so `affinity:[0..15]` is an exact 1-lane-per-core pinning.
Total compute is preserved (`13 × band_iters=40` still reaches `max_iter=512`). Same graph
for both conditions; only the `rt` pool differs.

| Condition (13-band, 16 lanes) | n | elapsed_s median | min | max | range | CoV | frame_hz |
|---|---|---|---|---|---|---|---|
| **Bm** no pool (16 lanes OS-migrated over 32 CPUs) | 9 | 30.63 | 30.58 | 30.91 | 0.32 | 0.3% | 12.54 |
| **Cm** `rt` pinned `[0..15]` (1 lane / physical core) | 9 | 30.70 | 30.64 | 30.74 | 0.10 | 0.1% | 12.51 |

**Cm vs Bm: +0.2% elapsed — statistically zero. Run-to-run spread cut ~3× (0.32 → 0.10 s;
CoV 0.3% → 0.1%).** Determinism digest identical, 384/384 frames in both.

## NICE policy under contention

The matched test above shows pinning costs ~0% throughput, but it also shows *niceness
would do nothing there* — with CPUs 16–31 idle there is no scheduling decision to bias.
**Niceness only matters when the CPU is contended.** To create that contention we run the
same matched 16-lane graph against a fixed background hog (`16 × yes` pinned to the same 16
physical cores `[0..15]`, at default nice 0) and vary only the rt pool's `policy`. Affinity
`[0..15]` and the workload are held constant, so the sole variable is the scheduling policy.

| Condition (13-band, 16 lanes, under 16-core hog) | policy | n | elapsed_s median | min | max | range | CoV | frame_hz | vs Nbase |
|---|---|---|---|---|---|---|---|---|---|
| **Nbase** rt pool, default | `Fair` (nice 0 — ties the hog) | 9 | **50.34** | 49.60 | 52.43 | 2.84 | 1.7% | 7.63 | baseline |
| **Nnice** rt pool favored | `Nice(-10)` | 9 | **32.77** | 32.50 | 33.21 | 0.71 | 0.6% | 11.71 | **−35% elapsed / +53% Hz** |
| **Nhi** rt pool yields | `Nice(10)` | 9 | **204.5** | 202.6 | 205.0 | 2.49 | 0.4% | 1.88 | **4.1× slower** |

*Reference: the same graph with **no hog** runs at 30.7 s / 12.5 Hz (Cm above).*

**Determinism gate: PASS.** All 27 runs produced the identical
`last_frame_digest = 0xe738bfff449bb8bd` — heavy CPU contention and the nice path do not
perturb numerical output.

### Interpretation

1. **`Nice(-10)` nearly cancels the contention.** Against a hog that halves Nbase to 7.63 Hz
   (1.64× slower than uncontended), favoring the pipeline recovers it to **11.71 Hz — within
   ~7% of the uncontended 12.5 Hz.** The control loop reclaims the cores it needs; the hog is
   relegated to the slack. This is the "protect the control loop under load" property, and
   unlike the no-contention Cm≈Bm tie it is a **large, unambiguous effect**.

2. **`Nice(10)` is the cautionary opposite.** Yielding the pipeline to the hog collapses it
   to **1.88 Hz (6.7× slower than uncontended, 4.1× slower than Nbase)** — the pipeline is
   starved whenever the hog wants the CPU. Positive nice on a *latency-critical* pool is a
   foot-gun; it is meant for genuinely sheddable background work, not the main loop.

3. **Niceness also tightened the favored run's spread** (range 2.84 → 0.71 s, ~4×), the same
   variance-reduction story affinity gave — here on top of the throughput win.

**Take-away:** niceness is a no-op without contention (don't expect it to help on an
unloaded box) but a *strong* lever once the CPU is oversubscribed. Give the rt/control pool a
favorable (negative) nice; reserve positive nice for work you are willing to starve.

### Privilege note

A favorable (negative) nice needs `RLIMIT_NICE` raised; with `on_error: Strict` an
unprivileged `Nice(-10)` hard-fails at startup. Grant it either persistently via
`/etc/security/limits.d/99-copper-rt.conf` (`<user> - nice -20`, applies on next login) or
in-session via `sudo prlimit --pid $$ --nice=40:40` (tmux-friendly, no re-login).
`ulimit -e` can misreport `0` even when the limit is actually raised, so `run_nice.sh`
probes the real capability (a negative `setpriority`) rather than trusting that readout.

## Conclusion

- **`parallel-rt` is the dominant lever here (~10.8× over serial).** It is pipeline
  parallelism: throughput comes from pipeline *depth* and CopperLists in flight, not from
  the thread pool.
- **The 29% "regression" in condition C was entirely a CPU-budget / oversubscription
  artifact, not a cost of pinning.** At matched depth (1 lane per core, same workload),
  pinning costs ~0% throughput.
- **The real, measurable effect of the affinity feature is lower timing variance** —
  ~3× tighter run-to-run spread even on this already-stable synthetic graph. That is the
  latency-determinism property the feature exists for; on a noisier real robot with
  contending background work the benefit would be larger.
- **Scheduling policy is contention-gated.** On an unloaded box `policy` does nothing
  (Cm≈Bm, and niceness would too). Under a competing CPU load it becomes a strong lever:
  `Nice(-10)` on the rt pool recovered throughput to within ~7% of uncontended (+53% Hz vs
  the same pool at default nice), while `Nice(10)` starved it 4×. Favor the control pool;
  never positively-nice it.
- **Take-away for users:** match `rt` pool `affinity` to the pipeline's stage count, not
  the other way round. Pinning more lanes than you have cores will cost throughput; pinning
  one lane per core buys predictability at no throughput cost. Once the CPU is contended,
  give the pool a favorable `Nice(-)`; a hard real-time policy (`Fifo`/`RoundRobin`, needs
  `CAP_SYS_NICE`/`RLIMIT_RTPRIO`) is the next step to bound tail latency under contention.

## Reproduce

```bash
cd examples/cu_parallel_mandelbrot
ROUNDS=2 REPS_A=1 REPS_B=4 REPS_C=4 bash bench/run_bench.sh
# raw samples -> bench/results_raw.csv

# Matched-depth affinity test (Bm/Cm):
ROUNDS=3 REPS=3 bash bench/run_matched.sh        # -> bench/results_matched.csv

# NICE under contention (Nbase/Nnice/Nhi). Needs RLIMIT_NICE for Nice(-10):
sudo prlimit --pid $$ --nice=40:40               # or limits.d + re-login
ROUNDS=3 REPS=3 bash bench/run_nice.sh           # -> bench/results_nice.csv
```
