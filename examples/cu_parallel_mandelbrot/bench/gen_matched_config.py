#!/usr/bin/env python3
"""Generate machine-matched copperconfig variants for the thread-pool benchmark.

The "matched" benchmark sizes pipeline *depth* to the host so there is exactly one
parallel-rt worker lane per physical core (src + N bands + frames + image_drain).
Hardcoding that for one machine makes the benchmark non-portable, so we derive it:

    bands      = cores - 3          # 3 non-band lanes: src, frames, image_drain
    band_iters = ceil(max_iter / bands)   # cumulative iters >= max_iter
    affinity   = [0 .. cores-1]

band_iters only needs cumulative reach >= max_iter, so the numerical output (and the
determinism digest) is invariant to `cores`: a 16-core run (13 bands x 40) and a
32-core run (29 bands x 18) both fully iterate every pixel and produce identical frames.

Emits four variants that differ only in the rt pool's scheduling policy:
    config_matched_base.ron     no thread_pools (OS-migrated baseline)
    config_matched_rt.ron       rt pool, affinity, Fair
    config_matched_nice.ron     rt pool, affinity, Nice(-10)  (favor the pipeline)
    config_matched_nice10.ron   rt pool, affinity, Nice(10)   (yield the pipeline)

Usage:
    ./gen_matched_config.py --cores 16            # explicit
    ./gen_matched_config.py                       # default: detected physical cores
"""
import argparse
import math
import os
import subprocess
import sys


def detect_physical_cores() -> int:
    """Physical cores (not hyperthreads). Falls back to os.cpu_count()//2."""
    try:
        out = subprocess.check_output(["lscpu", "-b", "-p=Core"], text=True)
        cores = {ln for ln in out.splitlines() if not ln.startswith("#")}
        if cores:
            return len(cores)
    except (OSError, subprocess.SubprocessError):
        pass
    return max(1, (os.cpu_count() or 2) // 2)


def band_task(idx: int, band_iters: int, last: bool) -> str:
    return f"""\
        (
            id: "band_{idx}",
            type: "tasks::MandelbrotIterBand",
            config: {{
                "stage_id": {idx + 1},
                "band_iters": {band_iters},
                "finalize": {"true" if last else "false"},
            }},
            logging: (enabled: false),
        ),"""


def rt_pool_block(cores: int, policy: str | None) -> str:
    affinity = ", ".join(str(c) for c in range(cores))
    policy_line = f"\n                policy: {policy}," if policy else ""
    return f"""\
    runtime: (
        thread_pools: [
            (
                id: "rt",
                threads: {cores},
                affinity: [{affinity}],{policy_line}
                on_error: Strict,
            ),
        ],
    ),
"""


def render(cores: int, max_iter: int, runtime_block: str) -> str:
    bands = cores - 3
    if bands < 1:
        sys.exit(f"need >= 4 cores to leave at least 1 band (got cores={cores})")
    band_iters = math.ceil(max_iter / bands)

    tasks = "\n".join(
        band_task(i, band_iters, last=(i == bands - 1)) for i in range(bands)
    )
    cnx = ['        ( src: "src", dst: "band_0", msg: "crate::payloads::MandelbrotStripe" ),']
    for i in range(bands - 1):
        cnx.append(
            f'        ( src: "band_{i}", dst: "band_{i + 1}", msg: "crate::payloads::MandelbrotStripe" ),'
        )
    last = bands - 1
    cnx.append(
        f'        ( src: "band_{last}", dst: "frames", msg: "crate::payloads::MandelbrotStripe", missions: ["log_only"] ),'
    )
    cnx.append(
        f'        ( src: "band_{last}", dst: "viewer_sink", msg: "crate::payloads::MandelbrotStripe", missions: ["viewer_live"] ),'
    )
    cnx.append(
        '        ( src: "frames", dst: "image_drain", msg: "cu_sensor_payloads::CuImage<Vec<u8>>", missions: ["log_only"] ),'
    )

    return f"""\
(
    missions: [
        (id: "log_only"),
        (id: "viewer_live"),
    ],
    tasks: [
        (
            id: "src",
            type: "tasks::MandelbrotStripeSource",
            config: {{
                "width": 1920,
                "height": 1080,
                "stripe_rows": 64,
                "frames": 384,
                "max_iter": {max_iter},
                "pool_slots": 32,
                "center_x": -0.743643887037151,
                "center_y": 0.131825904205330,
                "initial_span_x": 2.8,
                "zoom_ratio": 0.994133335507456,
            }},
            logging: (enabled: false),
        ),
{tasks}
        (
            id: "frames",
            type: "tasks::FrameAssembler",
            missions: ["log_only"],
            config: {{
                "width": 1920,
                "height": 1080,
                "frames": 384,
            }},
            logging: (enabled: true),
        ),
        (
            id: "viewer_sink",
            type: "tasks::ViewerFrameSink",
            missions: ["viewer_live"],
            config: {{
                "width": 1920,
                "height": 1080,
                "frames": 384,
            }},
            logging: (enabled: false),
        ),
        (
            id: "image_drain",
            type: "tasks::LoggedFrameDrain",
            missions: ["log_only"],
            logging: (enabled: false),
        ),
    ],
    cnx: [
{chr(10).join(cnx)}
    ],
{runtime_block}\
    logging: (
        enable_task_logging: true,
        copperlist_count: 32,
        slab_size_mib: 512,
        section_size_mib: 64,
        keyframe_interval: 1000000,
    ),
)
"""


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cores", type=int, default=None,
                    help="physical core count to match (default: auto-detect)")
    ap.add_argument("--max-iter", type=int, default=512,
                    help="Mandelbrot max iterations (default: 512)")
    ap.add_argument("--outdir", default=os.path.dirname(os.path.abspath(__file__)),
                    help="output directory (default: this script's dir)")
    args = ap.parse_args()

    cores = args.cores if args.cores is not None else detect_physical_cores()
    bands = cores - 3
    print(f"# matched config: cores={cores} -> bands={bands} band_iters={math.ceil(args.max_iter / bands)} affinity=[0..{cores - 1}]", file=sys.stderr)

    variants = {
        "config_matched_base.ron": "",
        "config_matched_rt.ron": rt_pool_block(cores, None),
        "config_matched_nice.ron": rt_pool_block(cores, "Nice(-10)"),
        "config_matched_nice10.ron": rt_pool_block(cores, "Nice(10)"),
    }
    for name, runtime_block in variants.items():
        path = os.path.join(args.outdir, name)
        with open(path, "w") as f:
            f.write(render(cores, args.max_iter, runtime_block))
        print(f"wrote {path}", file=sys.stderr)


if __name__ == "__main__":
    main()
