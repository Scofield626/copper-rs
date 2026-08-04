use circular_buffer::FixedCircularBuffer;
use cu29::bincode::de::read::Reader;
use cu29::bincode::de::{Decode, Decoder};
use cu29::bincode::enc::{Encode, Encoder};
use cu29::bincode::error::{DecodeError, EncodeError};
use cu29::prelude::*;

/// An augmented circular buffer that allows for time-based operations.
pub struct TimeboundCircularBuffer<const S: usize, P, M>
where
    P: CuMsgPayload,
    M: Metadata,
{
    pub inner: FixedCircularBuffer<CuStampedData<P, M>, S>,
}

#[allow(dead_code)]
fn extract_tov_time_left(tov: &Tov) -> Option<CuTime> {
    match tov {
        Tov::Time(time) => Some(*time),
        Tov::Range(range) => Some(range.start), // Use the start of the range for alignment
        Tov::None => None,
    }
}

fn extract_tov_time_right(tov: &Tov) -> Option<CuTime> {
    match tov {
        Tov::Time(time) => Some(*time),
        Tov::Range(range) => Some(range.end), // Use the end of the range for alignment
        Tov::None => None,
    }
}

/// Largest snapshot accepted for a single buffered message.
///
/// The runtime decodes keyframes with `bincode`'s `NoLimit` configuration, so
/// without a bound here a corrupted snapshot can declare any length it likes and
/// the process dies allocating it. 256 MiB is far above any realistic single
/// message (the biggest in-tree user is `cu_image_aligner`, which buffers whole
/// camera frames) and far below the point where the allocation itself is the
/// problem.
const MAX_MSG_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;

/// Budget handed to the inner decode of a message snapshot.
///
/// Careful: `bincode`'s limit counts *claimed* bytes, not wire bytes. Decoding a
/// container claims `len * size_of::<T>()` (see `Decoder::claim_container_read`),
/// so a `Vec<u64>` of small varints claims about eight times what it occupies on
/// the wire. The budget is therefore a multiple of the wire cap, sized for the
/// widest primitives in practice.
///
/// This is deliberately asymmetric with [`MAX_MSG_SNAPSHOT_BYTES`]: `freeze`
/// bounds wire bytes, `thaw` bounds claimed bytes, and no encoder-side limit
/// exists in `bincode` to make the two agree. A payload holding an enormous
/// collection of multi-byte elements could still be written and then refused on
/// the way back in. Both caps sit far above any realistic buffered message, so
/// the gap is a documented corner rather than a live concern.
const MAX_MSG_CLAIM_BYTES: usize = MAX_MSG_SNAPSHOT_BYTES * 8;

/// How much of a message snapshot is read before checking there is really more to
/// come. A large but honest payload costs a few extra reads; a bogus declared
/// length costs one buffer of this size and nothing more.
const SNAPSHOT_READ_CHUNK: usize = 8 * 1024;

fn encode_buffered_msg<P, E>(
    msg: &CuStampedData<P, CuMsgMetadata>,
    encoder: &mut E,
) -> Result<(), EncodeError>
where
    P: CuMsgPayload,
    E: Encoder,
{
    let bytes = cu29::bincode::encode_to_vec(msg, cu29::bincode::config::standard())?;
    // Bound what we write, so a snapshot cannot be larger than what
    // `decode_buffered_msg` is willing to read back by length.
    if bytes.len() > MAX_MSG_SNAPSHOT_BYTES {
        return Err(EncodeError::Other(
            "alignment buffer message is too large to snapshot",
        ));
    }
    Encode::encode(&bytes, encoder)
}

fn decode_buffered_msg<P, D>(
    decoder: &mut D,
) -> Result<CuStampedData<P, CuMsgMetadata>, DecodeError>
where
    P: CuMsgPayload,
    D: Decoder,
{
    // Same wire format as `Vec<u8>`: a u64 length prefix followed by the bytes.
    // Read it by hand rather than through `Vec::<u8>::decode`, which allocates the
    // whole declared length up front, before any bound is checked.
    let declared_len: u64 = Decode::decode(decoder)?;
    let declared_len =
        usize::try_from(declared_len).map_err(|_| DecodeError::OutsideUsizeRange(declared_len))?;
    if declared_len > MAX_MSG_SNAPSHOT_BYTES {
        return Err(DecodeError::LimitExceeded);
    }

    // Read in chunks so a length that the stream cannot actually satisfy fails on
    // end-of-input having allocated only what really arrived.
    let mut bytes = Vec::new();
    let mut remaining = declared_len;
    let mut chunk = [0u8; SNAPSHOT_READ_CHUNK];
    while remaining > 0 {
        let take = remaining.min(SNAPSHOT_READ_CHUNK);
        decoder.claim_bytes_read(take)?;
        decoder.reader().read(&mut chunk[..take])?;
        bytes.extend_from_slice(&chunk[..take]);
        remaining -= take;
    }

    // The inner decode needs the same treatment: a field inside the snapshot can
    // declare its own length, so it gets a limit rather than `NoLimit`. See
    // `MAX_MSG_CLAIM_BYTES` for why this budget is not the wire cap above.
    let (msg, bytes_read): (CuStampedData<P, CuMsgMetadata>, usize) =
        cu29::bincode::decode_from_slice(
            &bytes,
            cu29::bincode::config::standard().with_limit::<MAX_MSG_CLAIM_BYTES>(),
        )?;
    if bytes_read != bytes.len() {
        return Err(DecodeError::OtherString(
            "alignment buffer message snapshot had trailing bytes".to_string(),
        ));
    }
    Ok(msg)
}

impl<const S: usize, P> Default for TimeboundCircularBuffer<S, P, CuMsgMetadata>
where
    P: CuMsgPayload,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<const S: usize, P> TimeboundCircularBuffer<S, P, CuMsgMetadata>
where
    P: CuMsgPayload,
{
    pub fn new() -> Self {
        Self {
            // It is assumed to be sorted by time with non overlapping ranges if they are Tov::Range
            inner: FixedCircularBuffer::<CuStampedData<P, CuMsgMetadata>, S>::new(),
        }
    }

    /// Gets a slice of messages that fall within the given time range.
    /// In case of a Tov::Range, the message is included if its start and end time fall within the range.
    pub fn iter_window(
        &self,
        start_time: CuTime,
        end_time: CuTime,
    ) -> impl Iterator<Item = &CuStampedData<P, CuMsgMetadata>> {
        self.inner.iter().filter(move |msg| match msg.tov {
            Tov::Time(time) => time >= start_time && time <= end_time,
            Tov::Range(range) => range.start >= start_time && range.end <= end_time,
            _ => false,
        })
    }

    /// Remove all the messages that are older than the given time horizon.
    pub fn purge(&mut self, time_horizon: CuTime) {
        // Find the index of the first element that should be retained
        let drain_end = self
            .inner
            .iter()
            .position(|msg| match msg.tov {
                Tov::Time(time) => time >= time_horizon,
                Tov::Range(range) => range.end >= time_horizon,
                _ => false,
            })
            .unwrap_or(self.inner.len()); // If none match, drain the entire buffer

        // Drain all elements before the `drain_end` index
        self.inner.drain(..drain_end);
    }

    /// Get the most recent time of the messages in the buffer.
    pub fn most_recent_time(&self) -> CuResult<Option<CuTime>> {
        let mut latest: Option<CuTime> = None;
        for msg in self.inner.iter() {
            let time = extract_tov_time_right(&msg.tov).ok_or_else(|| {
                CuError::from("Trying to align temporal data with no time information")
            })?;
            latest = Some(latest.map_or(time, |current_max| current_max.max(time)));
        }
        Ok(latest)
    }

    /// Push a message into the buffer.
    pub fn push(&mut self, msg: CuStampedData<P, CuMsgMetadata>) {
        self.inner.push_back(msg);
    }

    pub fn freeze<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
        Encode::encode(&(self.inner.len() as u64), encoder)?;
        for msg in self.inner.iter() {
            encode_buffered_msg(msg, encoder)?;
        }
        Ok(())
    }

    pub fn thaw<D: Decoder>(&mut self, decoder: &mut D) -> Result<(), DecodeError> {
        let len: u64 = Decode::decode(decoder)?;
        let len = usize::try_from(len).map_err(|_| {
            DecodeError::OtherString("alignment buffer length does not fit usize".to_string())
        })?;
        if len > S {
            return Err(DecodeError::ArrayLengthMismatch {
                required: S,
                found: len,
            });
        }

        self.inner.clear();
        for _ in 0..len {
            self.inner.push_back(decode_buffered_msg(decoder)?);
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! alignment_buffers {
    ($struct_name:ident, $($name:ident: TimeboundCircularBuffer<$size:expr, CuStampedData<$payload:ty, CuMsgMetadata>>),*) => {
        struct $struct_name {
            target_alignment_window: cu29::clock::CuDuration, // size of the most recent data window to align
            stale_data_horizon: cu29::clock::CuDuration,  // time horizon for purging stale data
            $(pub $name: $crate::buffers::TimeboundCircularBuffer<$size, $payload, CuMsgMetadata>),*
        }

        impl $struct_name {
            pub fn new(target_alignment_window: cu29::clock::CuDuration, stale_data_horizon: cu29::clock::CuDuration) -> Self {
                Self {
                    target_alignment_window,
                    stale_data_horizon,
                    $($name: $crate::buffers::TimeboundCircularBuffer::<$size, $payload, CuMsgMetadata>::new()),*
                }
            }

            /// Call this to be sure we discard the old/ non relevant data
            #[allow(dead_code)]
            pub fn purge(&mut self, now: cu29::clock::CuTime) {
                let horizon_time = now - self.stale_data_horizon;
                // purge all the stale data from the TimeboundCircularBuffers first
                $(self.$name.purge(horizon_time);)*
            }

            /// Get the most recent set of aligned data from all the buffers matching the constraints set at construction.
            #[allow(dead_code)]
            pub fn get_latest_aligned_data(
                &mut self,
            ) -> Option<($(impl Iterator<Item = &cu29::cutask::CuStampedData<$payload, CuMsgMetadata>>),*)> {
                // Now find the min of the max of the last time for all buffers
                // meaning the most recent time at which all buffers have data
                let most_recent_time = [
                    $(self.$name.most_recent_time().unwrap_or(None)),*
                ]
                .into_iter()
                .flatten()
                .min()?;

                let time_to_get_complete_window = most_recent_time - self.target_alignment_window;
                Some(($(self.$name.iter_window(time_to_get_complete_window, most_recent_time)),*))
            }

            #[allow(dead_code)]
            pub fn freeze<E: cu29::bincode::enc::Encoder>(&self, encoder: &mut E) -> Result<(), cu29::bincode::error::EncodeError> {
                $(self.$name.freeze(encoder)?;)*
                Ok(())
            }

            #[allow(dead_code)]
            pub fn thaw<D: cu29::bincode::de::Decoder>(&mut self, decoder: &mut D) -> Result<(), cu29::bincode::error::DecodeError> {
                $(self.$name.thaw(decoder)?;)*
                Ok(())
            }
        }
    };
}

pub use alignment_buffers;

#[cfg(test)]
mod tests {
    use super::*;
    use cu29::clock::Tov;
    use std::time::Duration;

    type TestBuffer = TimeboundCircularBuffer<4, u32, CuMsgMetadata>;

    /// Drives `thaw` through the normal bincode entry point.
    struct Thawed(TestBuffer);

    impl Decode<()> for Thawed {
        fn decode<D: Decoder<Context = ()>>(decoder: &mut D) -> Result<Self, DecodeError> {
            let mut buffer = TestBuffer::new();
            buffer.thaw(decoder)?;
            Ok(Thawed(buffer))
        }
    }

    /// Drives `freeze` through the normal bincode entry point.
    struct Frozen<'a>(&'a TestBuffer);

    impl Encode for Frozen<'_> {
        fn encode<E: Encoder>(&self, encoder: &mut E) -> Result<(), EncodeError> {
            self.0.freeze(encoder)
        }
    }

    /// A corrupted snapshot must be rejected, not turned into a multi-gigabyte
    /// allocation. The runtime decodes with `NoLimit`, so the bound has to live here.
    #[test]
    fn thaw_rejects_a_bogus_message_length() {
        let config = cu29::bincode::config::standard();

        // A length past the per-message cap is rejected before anything is read.
        let bytes = cu29::bincode::encode_to_vec((1u64, 5_000_000_000u64), config).unwrap();
        let Err(err) = cu29::bincode::decode_from_slice::<Thawed, _>(&bytes, config) else {
            panic!("a snapshot claiming 5 GB must be rejected");
        };
        assert!(
            matches!(err, DecodeError::LimitExceeded),
            "expected the per-message cap to reject it, got {err:?}"
        );

        // A length under the cap that the stream cannot satisfy fails on
        // end-of-input, having allocated only what actually arrived. The old code
        // reported the same error kind here, so this half guards the chunked-read
        // path rather than the cap: what changed is that 64 MB is no longer
        // allocated up front before the failure.
        let bytes = cu29::bincode::encode_to_vec((1u64, 64_000_000u64), config).unwrap();
        let Err(err) = cu29::bincode::decode_from_slice::<Thawed, _>(&bytes, config) else {
            panic!("a snapshot promising 64 MB of absent bytes must be rejected");
        };
        assert!(
            matches!(err, DecodeError::UnexpectedEnd { .. }),
            "expected an end-of-input error, got {err:?}"
        );
    }

    /// The chunked read must still round-trip an honest snapshot byte for byte.
    #[test]
    fn freeze_thaw_round_trips() {
        let mut buffer = TestBuffer::new();
        for (i, payload) in [11u32, 22, 33].into_iter().enumerate() {
            let mut msg = CuStampedData::<u32, CuMsgMetadata>::new(Some(payload));
            msg.tov = Tov::Time(Duration::from_secs(i as u64 + 1).into());
            buffer.push(msg);
        }

        let config = cu29::bincode::config::standard();
        let bytes = cu29::bincode::encode_to_vec(Frozen(&buffer), config).unwrap();
        let (Thawed(restored), _) =
            cu29::bincode::decode_from_slice::<Thawed, _>(&bytes, config).unwrap();

        assert_eq!(restored.inner.len(), buffer.inner.len());
        for (a, b) in restored.inner.iter().zip(buffer.inner.iter()) {
            assert_eq!(a.payload(), b.payload());
            assert_eq!(a.tov, b.tov);
        }
    }

    #[test]
    fn simple_init_test() {
        alignment_buffers!(AlignmentBuffers, buffer1: TimeboundCircularBuffer<10, CuStampedData<u32, CuMsgMetadata>>, buffer2: TimeboundCircularBuffer<12, CuStampedData<u64, CuMsgMetadata>>);

        let buffers =
            AlignmentBuffers::new(Duration::from_secs(1).into(), Duration::from_secs(2).into());
        assert_eq!(buffers.buffer1.inner.capacity(), 10);
        assert_eq!(buffers.buffer2.inner.capacity(), 12);
    }

    #[test]
    fn purge_test() {
        alignment_buffers!(AlignmentBuffers, buffer1: TimeboundCircularBuffer<10, CuStampedData<u32, CuMsgMetadata>>, buffer2: TimeboundCircularBuffer<12, CuStampedData<u32, CuMsgMetadata>>);

        let mut buffers =
            AlignmentBuffers::new(Duration::from_secs(1).into(), Duration::from_secs(2).into());

        let mut msg1 = CuStampedData::new(Some(1));
        msg1.tov = Tov::Time(Duration::from_secs(1).into());
        buffers.buffer1.inner.push_back(msg1.clone());
        buffers.buffer2.inner.push_back(msg1);
        // within the horizon
        buffers.purge(Duration::from_secs(2).into());
        assert_eq!(buffers.buffer1.inner.len(), 1);
        assert_eq!(buffers.buffer2.inner.len(), 1);
        // outside the horizon
        buffers.purge(Duration::from_secs(5).into());
        assert_eq!(buffers.buffer1.inner.len(), 0);
        assert_eq!(buffers.buffer2.inner.len(), 0);
    }

    #[test]
    fn empty_buffers_test() {
        alignment_buffers!(
            AlignmentBuffers,
            buffer1: TimeboundCircularBuffer<10, CuStampedData<u32, CuMsgMetadata>>,
            buffer2: TimeboundCircularBuffer<12, CuStampedData<u32, CuMsgMetadata>>
        );

        let mut buffers = AlignmentBuffers::new(
            Duration::from_secs(2).into(), // 2-second alignment window
            Duration::from_secs(5).into(), // 5-second stale data horizon
        );

        // Advance time to 10 seconds
        assert!(buffers.get_latest_aligned_data().is_none());
    }

    #[test]
    fn horizon_and_window_alignment_test() {
        alignment_buffers!(
            AlignmentBuffers,
            buffer1: TimeboundCircularBuffer<10, CuStampedData<u32, CuMsgMetadata>>,
            buffer2: TimeboundCircularBuffer<12, CuStampedData<u32, CuMsgMetadata>>
        );

        let mut buffers = AlignmentBuffers::new(
            Duration::from_secs(2).into(), // 2-second alignment window
            Duration::from_secs(5).into(), // 5-second stale data horizon
        );

        // Insert messages with timestamps
        let mut msg1 = CuStampedData::new(Some(1));
        msg1.tov = Tov::Time(Duration::from_secs(1).into());
        buffers.buffer1.inner.push_back(msg1.clone());
        buffers.buffer2.inner.push_back(msg1);

        let mut msg2 = CuStampedData::new(Some(3));
        msg2.tov = Tov::Time(Duration::from_secs(3).into());
        buffers.buffer2.inner.push_back(msg2);

        let mut msg3 = CuStampedData::new(Some(4));
        msg3.tov = Tov::Time(Duration::from_secs(4).into());
        buffers.buffer1.inner.push_back(msg3.clone());
        buffers.buffer2.inner.push_back(msg3);

        // Advance time to 7 seconds; horizon is 7 - 5 = everything 2+ should stay
        let now = Duration::from_secs(7).into();
        // Emulate a normal workflow here.
        buffers.purge(now);
        if let Some((iter1, iter2)) = buffers.get_latest_aligned_data() {
            let collected1: Vec<_> = iter1.collect();
            let collected2: Vec<_> = iter2.collect();

            // Verify only messages within the alignment window [5, 7] are returned
            assert_eq!(collected1.len(), 1);
            assert_eq!(collected2.len(), 2);

            assert_eq!(collected1[0].payload(), Some(&4));
            assert_eq!(collected2[0].payload(), Some(&3));
            assert_eq!(collected2[1].payload(), Some(&4));
        } else {
            panic!("Expected aligned data, but got None");
        }

        // Ensure older messages outside the horizon [>2 seconds] are purged
        assert_eq!(buffers.buffer1.inner.len(), 1);
        assert_eq!(buffers.buffer2.inner.len(), 2);
    }
}
