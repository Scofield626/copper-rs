#![doc = include_str!("../README.md")]

pub mod buffers;

/// Define a task that aligns incoming messages based on their timestamps
/// See module doc for use.
#[macro_export]
macro_rules! define_task {
    ($name:ident, $($index:tt => { $mis:expr, $mos:expr, $p:ty }),+) => {

       paste::paste! {
            #[allow(unused_imports)]
            use cu29::prelude::*;

            $crate::buffers::alignment_buffers!(
                AlignmentBuffers,
                $(
                    [<buffer $index>]: TimeboundCircularBuffer<$mis, CuStampedData<$p, CuMsgMetadata>>
                ),*
            );
        }

        #[derive(Reflect)]
        #[reflect(from_reflect = false)]
        pub struct $name {
            #[reflect(ignore)]
            aligner: AlignmentBuffers,
        }

        impl Freezable for $name {
            fn freeze<E: cu29::bincode::enc::Encoder>(&self, encoder: &mut E) -> Result<(), cu29::bincode::error::EncodeError> {
                self.aligner.freeze(encoder)
            }

            fn thaw<D: cu29::bincode::de::Decoder>(&mut self, decoder: &mut D) -> Result<(), cu29::bincode::error::DecodeError> {
                self.aligner.thaw(decoder)
            }
        }

        impl CuTask for $name {
    type Resources<'r> = ();
            type Input<'m> = input_msg!('m, $($p),*);
            type Output<'m> = output_msg!(($(
                CuArray<$p, { $mos }>
            ),*));

            fn new(config: Option<&ComponentConfig>, _resources: Self::Resources<'_>) -> CuResult<Self>
            where
                Self: Sized,
            {
                let config = config.ok_or_else(|| CuError::from("Config Missing"))?;
                let target_alignment_window_ms: u64 = config
                    .get::<u32>("target_alignment_window_ms")?
                    .ok_or_else(|| CuError::from("Missing target_alignment_window"))?
                    .into();
                let stale_data_horizon_ms: u64 = config
                    .get::<u32>("stale_data_horizon_ms")?
                    .ok_or_else(|| CuError::from("Missing stale_data_horizon"))?
                    .into();

                let target_alignment_window =
                    cu29_clock::CuDuration(target_alignment_window_ms * 1_000_000);
                let stale_data_horizon =
                    cu29_clock::CuDuration(stale_data_horizon_ms * 1_000_000);

                Ok(Self {
                    aligner: AlignmentBuffers::new(target_alignment_window, stale_data_horizon),
                })
            }

            fn preprocess(&mut self, ctx: &CuContext) -> CuResult<()> {
                self.aligner.purge(ctx.now());
                Ok(())
            }

            fn process(
                &mut self,
                _ctx: &CuContext,
                input: &Self::Input<'_>,
                output: &mut Self::Output<'_>,
            ) -> CuResult<()> {
                // Add the incoming data into the buffers.
                // input is a tuple of &CuMsg<T> for each T in the input.
                // A tick where the upstream task had nothing to emit carries a tov
                // but no payload. It holds no data to align, so it must not take a
                // slot in the fixed-size buffer nor advance the alignment window.
                paste::paste! {
                    $(
                        if input.$index.payload().is_some() {
                            self.aligner.[<buffer $index>].push(input.$index.clone());
                        }
                    )*
                }


                // this is a tuple of iterators of CuStampedDataSet
                let Some(tuple_of_iters) = self.aligner.get_latest_aligned_data() else {
                    return Ok(());
                };

                // Populate the CuArray fields in the output message.
                // `TimeboundCircularBuffer::push` is public, so skip payload-less
                // messages here too rather than unwrapping them.
                let output_payload = output.payload_mut().get_or_insert_with(Default::default);
                $(
                    output_payload.$index.fill_from_iter(tuple_of_iters.$index.filter_map(|msg| msg.payload().cloned()));
                )*
                Ok(())
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use cu29::prelude::*;

    define_task!(AlignerTask, 0 => { 10, 5, f32 }, 1 => { 5, 10, i32 });
    #[test]
    fn test_aligner_smoketest() {
        let mut config = ComponentConfig::default();
        config.set("target_alignment_window_ms", 1000);
        config.set("stale_data_horizon_ms", 2000);
        let mut aligner = AlignerTask::new(Some(&config), ()).unwrap();
        let m1 = CuStampedData::<f32, CuMsgMetadata>::default();
        let m2 = CuStampedData::<i32, CuMsgMetadata>::default();
        let input: <AlignerTask as CuTask>::Input<'_> = (&m1, &m2);
        let m3 = CuStampedData::<(CuArray<f32, 5>, CuArray<i32, 10>), CuMsgMetadata>::default();
        let mut output: <AlignerTask as CuTask>::Output<'_> = m3;

        let ctx = CuContext::new_with_clock();
        let result = aligner.process(&ctx, &input, &mut output);
        assert!(result.is_ok());
    }
    /// A task that had nothing to emit still ticks, so a message can carry a tov
    /// with no payload. Such a message used to reach `payload().unwrap()` and abort
    /// the process.
    #[test]
    fn test_aligner_tolerates_payload_less_ticks() {
        let mut config = ComponentConfig::default();
        config.set("target_alignment_window_ms", 100);
        config.set("stale_data_horizon_ms", 1000);
        let mut aligner = AlignerTask::new(Some(&config), ()).unwrap();
        let ctx = CuContext::new_with_clock();

        let tov = Tov::Time(CuTime::from_millis(100));
        let mut empty = CuStampedData::<f32, CuMsgMetadata>::new(None);
        empty.tov = tov;
        let mut present = CuStampedData::<i32, CuMsgMetadata>::new(Some(7));
        present.tov = tov;

        let mut output =
            CuStampedData::<(CuArray<f32, 5>, CuArray<i32, 10>), CuMsgMetadata>::default();
        aligner
            .process(&ctx, &(&empty, &present), &mut output)
            .unwrap();

        // The payload-less tick contributes nothing and takes no buffer slot, so
        // that stream's array comes back empty. An empty array is already a normal
        // outcome here: `iter_window` selects on time, so a stream whose data all
        // falls outside the window yields nothing even when every message it sent
        // carried a payload. Consumers have to handle that either way.
        let payload = output.payload().unwrap();
        assert_eq!(payload.0.len(), 0);
        assert_eq!(payload.1.as_slice(), &[7]);

        // Once real data arrives on that stream, both align as usual.
        let tov = Tov::Time(CuTime::from_millis(150));
        let mut left = CuStampedData::<f32, CuMsgMetadata>::new(Some(1.5));
        left.tov = tov;
        let mut right = CuStampedData::<i32, CuMsgMetadata>::new(Some(9));
        right.tov = tov;

        let mut output =
            CuStampedData::<(CuArray<f32, 5>, CuArray<i32, 10>), CuMsgMetadata>::default();
        aligner
            .process(&ctx, &(&left, &right), &mut output)
            .unwrap();

        let payload = output.payload().unwrap();
        assert_eq!(payload.0.as_slice(), &[1.5]);
        assert_eq!(payload.1.as_slice(), &[7, 9]);
    }

    mod string_payload {
        use super::*;

        define_task!(StringAlignerTask, 0 => { 4, 4, String }, 1 => { 4, 4, String });

        #[test]
        fn test_aligner_string_payload() {
            let mut config = ComponentConfig::default();
            config.set("target_alignment_window_ms", 10);
            config.set("stale_data_horizon_ms", 1000);
            let mut aligner = StringAlignerTask::new(Some(&config), ()).unwrap();

            let mut left = CuStampedData::<String, CuMsgMetadata>::new(Some("left".to_string()));
            let mut right = CuStampedData::<String, CuMsgMetadata>::new(Some("right".to_string()));
            let tov_time = CuTime::from_millis(100);
            left.tov = Tov::Time(tov_time);
            right.tov = Tov::Time(tov_time);

            let input: <StringAlignerTask as CuTask>::Input<'_> = (&left, &right);
            let mut output =
                CuStampedData::<(CuArray<String, 4>, CuArray<String, 4>), CuMsgMetadata>::default();

            let ctx = CuContext::new_with_clock();
            aligner.process(&ctx, &input, &mut output).unwrap();

            let payload = output.payload().unwrap();
            assert_eq!(payload.0.len(), 1);
            assert_eq!(payload.1.len(), 1);
            assert_eq!(payload.0.as_slice()[0], "left");
            assert_eq!(payload.1.as_slice()[0], "right");
        }
    }
}
