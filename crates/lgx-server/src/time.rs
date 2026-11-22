//! Monotonic clock shared by the video and audio timestamps.

/// Microseconds from `CLOCK_MONOTONIC`. Video capture and the PipeWire audio
/// callback must use the same clock, otherwise the client cannot align them.
pub fn monotonic_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}
