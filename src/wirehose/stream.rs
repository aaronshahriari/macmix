use pulp::{Arch, Simd, WithSimd};

/// Trait for processing peaks in order to implement effects like ballistics.
pub trait PeakProcessor: Send + Sync {
    fn process_peak(
        &self,
        current_peak: f32,
        previous_peak: f32,
        sample_count: u32,
        sample_rate: u32,
    ) -> f32;
}

impl<F> PeakProcessor for F
where
    F: Fn(f32, f32, u32, u32) -> f32 + Send + Sync,
{
    fn process_peak(
        &self,
        current_peak: f32,
        previous_peak: f32,
        sample_count: u32,
        sample_rate: u32,
    ) -> f32 {
        self(current_peak, previous_peak, sample_count, sample_rate)
    }
}

/// Find the maximum absolute sample value in a buffer (SIMD-accelerated).
/// Used by the CoreAudio metering path in Phase 2.
#[allow(dead_code)]
pub fn find_peak(samples: &[f32]) -> f32 {
    struct Max<'a>(&'a [f32]);
    impl WithSimd for Max<'_> {
        type Output = f32;

        #[inline(always)]
        fn with_simd<S: Simd>(self, simd: S) -> Self::Output {
            let v = self.0;

            let (head, tail) = S::as_simd_f32s(v);

            let mut head_max = simd.splat_f32s(0.0);
            for x in head {
                head_max = simd.max_f32s(head_max, simd.abs_f32s(*x));
            }
            let head_max = head_max;

            let mut tail_max = simd.reduce_max_f32s(head_max);
            for x in tail {
                tail_max = tail_max.max(x.abs());
            }

            tail_max
        }
    }

    Arch::new().dispatch(Max(samples))
}
