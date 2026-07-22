#[global_allocator]
static BENCH_ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::time::Duration;

use criterion::Criterion;

pub const WARM_UP_MS: u64 = 100;
pub const MEASUREMENT_MS: u64 = 500;
pub const MAX_CASE_MS: u64 = 2_000;

const _: () = assert!(WARM_UP_MS + MEASUREMENT_MS < MAX_CASE_MS);

/// Every microbenchmark is intentionally bounded well below the repository's
/// two-second-per-case ceiling. CLI overrides may shorten these values, but the
/// checked-in defaults remain the reproducible comparison configuration.
pub fn criterion() -> Criterion {
    Criterion::default()
        .significance_level(0.01)
        .sample_size(20)
        .nresamples(10_000)
        .warm_up_time(Duration::from_millis(WARM_UP_MS))
        .measurement_time(Duration::from_millis(MEASUREMENT_MS))
}
