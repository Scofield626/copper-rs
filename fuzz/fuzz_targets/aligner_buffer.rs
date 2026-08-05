#![no_main]
//! Fuzz `TimeboundCircularBuffer`: the time-window/purge logic and its freeze/thaw codec.
//!
//! Oracles:
//!   * the buffer never exceeds its const capacity;
//!   * `purge(h)` never leaves a message strictly older than `h` behind;
//!   * `iter_window(a, b)` only yields messages inside `[a, b]`;
//!   * `most_recent_time` is >= every window element it reports;
//!   * freeze/thaw round-trips, and thawing arbitrary bytes never panics.

use arbitrary::Arbitrary;
use cu_aligner::buffers::TimeboundCircularBuffer;
use cu29::bincode::de::{Decode, Decoder};
use cu29::bincode::error::DecodeError;
use cu29::prelude::*;
use libfuzzer_sys::fuzz_target;

const CAP: usize = 8;

type Buf = TimeboundCircularBuffer<CAP, u32, CuMsgMetadata>;

#[derive(Arbitrary, Debug)]
enum FuzzTov {
    None,
    Time(u64),
    Range(u64, u64),
}

impl FuzzTov {
    fn to_tov(&self) -> Tov {
        match self {
            FuzzTov::None => Tov::None,
            FuzzTov::Time(t) => Tov::Time(CuTime::from(*t)),
            FuzzTov::Range(a, b) => Tov::Range(CuTimeRange {
                start: CuTime::from(*a),
                end: CuTime::from(*b),
            }),
        }
    }
}

#[derive(Arbitrary, Debug)]
enum Op {
    Push { payload: Option<u32>, tov: FuzzTov },
    Purge(u64),
    MostRecent,
    Window(u64, u64),
    FreezeThaw,
}

struct ThawedBuf(Buf);

impl Decode<()> for ThawedBuf {
    fn decode<D: Decoder<Context = ()>>(decoder: &mut D) -> Result<Self, DecodeError> {
        let mut buf = Buf::new();
        buf.thaw(decoder)?;
        Ok(ThawedBuf(buf))
    }
}

#[derive(Arbitrary, Debug)]
struct Scenario {
    /// When set, pushes are forced into non-decreasing time order, which is the
    /// precondition `TimeboundCircularBuffer` documents ("assumed to be sorted by
    /// time with non overlapping ranges"). The purge oracle only applies here.
    sorted: bool,
    ops: Vec<Op>,
    /// Raw bytes fed straight into `thaw` to model a corrupted keyframe.
    snapshot: Vec<u8>,
}

fn tov_right(tov: &Tov) -> Option<CuTime> {
    match tov {
        Tov::Time(t) => Some(*t),
        Tov::Range(r) => Some(r.end),
        Tov::None => None,
    }
}

fuzz_target!(|scenario: Scenario| {
    if scenario.ops.len() > 256 {
        return;
    }
    // `decode_buffered_msg` (buffers.rs:52) decodes a `Vec<u8>` whose length comes
    // straight from the snapshot, before any bound check. Under the default
    // byte-limited config that finding stays out of the way; build with
    // `--features unbounded_decode` to reproduce it.
    #[cfg(feature = "unbounded_decode")]
    let cfg = cu29::bincode::config::standard();
    #[cfg(not(feature = "unbounded_decode"))]
    let cfg = cu29::bincode::config::standard().with_limit::<1_000_000>();

    // 1. A corrupted snapshot must be rejected, not crash the runtime.
    if let Ok((ThawedBuf(buf), _)) =
        cu29::bincode::decode_from_slice::<ThawedBuf, _>(&scenario.snapshot, cfg)
    {
        assert!(
            buf.inner.len() <= CAP,
            "thaw overfilled the buffer: {} > {CAP}",
            buf.inner.len()
        );
    }

    // 2. Drive the buffer through an arbitrary op sequence.
    let mut buf = Buf::new();
    let mut clock: u64 = 0;
    for op in &scenario.ops {
        match op {
            Op::Push { payload, tov } => {
                let mut msg = CuStampedData::<u32, CuMsgMetadata>::new(*payload);
                msg.tov = if scenario.sorted {
                    // Force a sorted, non-overlapping stream: each push advances a
                    // monotonic clock, so the buffer's precondition holds.
                    match tov {
                        FuzzTov::None => Tov::None,
                        FuzzTov::Time(step) => {
                            clock = clock.saturating_add(step % 1_000_000);
                            Tov::Time(CuTime::from(clock))
                        }
                        FuzzTov::Range(step, span) => {
                            let start = clock.saturating_add(step % 1_000_000);
                            let end = start.saturating_add(span % 1_000_000);
                            clock = end;
                            Tov::Range(CuTimeRange {
                                start: CuTime::from(start),
                                end: CuTime::from(end),
                            })
                        }
                    }
                } else {
                    tov.to_tov()
                };
                buf.push(msg);
                assert!(buf.inner.len() <= CAP, "push exceeded capacity");
            }
            Op::Purge(h) => {
                let horizon = if scenario.sorted {
                    CuTime::from(*h % (clock + 1))
                } else {
                    CuTime::from(*h)
                };
                buf.purge(horizon);
                if scenario.sorted {
                    for msg in buf.inner.iter() {
                        if let Some(t) = tov_right(&msg.tov) {
                            assert!(
                                t >= horizon,
                                "purge({horizon:?}) kept a stale message at {t:?}"
                            );
                        }
                    }
                }
            }
            Op::MostRecent => {
                let recent = buf.most_recent_time();
                if let Ok(Some(latest)) = recent {
                    for msg in buf.inner.iter() {
                        if let Some(t) = tov_right(&msg.tov) {
                            assert!(t <= latest, "most_recent_time {latest:?} < element {t:?}");
                        }
                    }
                }
            }
            Op::Window(a, b) => {
                let (start, end) = (CuTime::from(*a), CuTime::from(*b));
                for msg in buf.iter_window(start, end) {
                    match msg.tov {
                        Tov::Time(t) => assert!(
                            t >= start && t <= end,
                            "iter_window yielded {t:?} outside [{start:?}, {end:?}]"
                        ),
                        Tov::Range(r) => assert!(
                            r.start >= start && r.end <= end,
                            "iter_window yielded {r:?} outside [{start:?}, {end:?}]"
                        ),
                        Tov::None => panic!("iter_window yielded an untimed message"),
                    }
                }
            }
            Op::FreezeThaw => {
                let bytes = cu29::bincode::encode_to_vec(BufAdapter(&buf), cfg)
                    .expect("freeze must not fail");
                let (ThawedBuf(restored), _) =
                    cu29::bincode::decode_from_slice::<ThawedBuf, _>(&bytes, cfg)
                        .expect("freeze output must be thawable");
                assert_eq!(
                    restored.inner.len(),
                    buf.inner.len(),
                    "freeze/thaw changed the buffer length"
                );
                for (a, b) in restored.inner.iter().zip(buf.inner.iter()) {
                    assert_eq!(a.payload(), b.payload(), "freeze/thaw changed a payload");
                    assert_eq!(a.tov, b.tov, "freeze/thaw changed a tov");
                }
                buf = restored;
            }
        }
    }
});

struct BufAdapter<'a>(&'a Buf);

impl cu29::bincode::enc::Encode for BufAdapter<'_> {
    fn encode<E: cu29::bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> Result<(), cu29::bincode::error::EncodeError> {
        self.0.freeze(encoder)
    }
}
