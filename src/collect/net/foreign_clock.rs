//! Timestamps taken in somebody else's clock domain.
//!
//! `CLOCK_MONOTONIC` is the only domain in this tool, and two sources in this
//! family produce stamps that are not in it: a packet socket stamps a frame
//! from the realtime clock, and a NIC's PTP hardware clock runs on its own
//! oscillator entirely. Both have to be converted at the boundary, and the
//! conversion has to say how wrong it could be -- which is what `ts_err_ns` on
//! an event is for.
//!
//! The calibration is a sandwich: read `CLOCK_MONOTONIC`, read the other
//! clock, read `CLOCK_MONOTONIC` again. The offset between the two domains is
//! the middle reading minus the midpoint of the outer two, and the width of
//! that window is the first term of the error. It is a few microseconds on this
//! machine.
//!
//! The second term is drift. Between the moment a stamp was taken and the
//! moment the calibration was, the two clocks may have moved apart. For the
//! realtime clock the kernel bounds that: `MAXFREQ` caps what `adjtimex` will
//! slew to 500 parts per million, so an offset measured a tick ago is wrong by
//! at most the tick times 500 ppm. For a free-running PHC nothing bounds it,
//! and that is reported as a measurement rather than assumed away.
//!
//! What no bound covers is a *step*: `settimeofday` can move the realtime clock
//! by any amount at any moment. It cannot be predicted, but it can be caught,
//! because two calibrations that disagree by more than the slew allows can only
//! have had one between them. That is a loss with a name, not a silent shift of
//! every timestamp in the window.

use std::io;

use crate::clock::monotonic_ns;

/// The kernel's own ceiling on how fast it will slew a clock: `MAXFREQ` in
/// `kernel/time/ntp.c`, 500 000 parts per billion.
pub const MAX_SLEW_PPM: u64 = 500;

/// One paired reading of another clock and `CLOCK_MONOTONIC`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Calibration {
    /// The other clock, as it read.
    pub foreign_ns: u64,
    /// `CLOCK_MONOTONIC` at the midpoint of the window the pair was read in.
    pub monotonic_ns: u64,
    /// How long the three reads took. The conversion cannot be more precise
    /// than this, whatever else is true.
    pub window_ns: u64,
}

impl Calibration {
    /// Reads `clock` between two readings of `CLOCK_MONOTONIC`.
    pub fn take(clock: libc::clockid_t) -> io::Result<Self> {
        let before = monotonic_ns();
        let foreign_ns = read_clock(clock)?;
        let after = monotonic_ns();
        Ok(Self {
            foreign_ns,
            monotonic_ns: before + (after - before) / 2,
            window_ns: after - before,
        })
    }

    /// What has to be subtracted from a foreign stamp to land in
    /// `CLOCK_MONOTONIC`.
    ///
    /// Signed, and not small: the realtime clock counts from 1970 and the
    /// monotonic one from boot, so the offset between them is decades.
    pub fn offset_ns(&self) -> i128 {
        self.foreign_ns as i128 - self.monotonic_ns as i128
    }

    /// Converts a stamp taken in the other domain, and says how far off the
    /// answer may be.
    ///
    /// `drift_ppm` is what the caller knows about the two clocks' rates: the
    /// kernel's slew ceiling for the realtime clock, and whatever was measured
    /// for a free-running one.
    pub fn convert(&self, foreign_ns: u64, drift_ppm: u64) -> (u64, u32) {
        let monotonic = foreign_ns as i128 - self.offset_ns();
        let monotonic = monotonic.max(0) as u64;
        let age_ns = self.monotonic_ns.abs_diff(monotonic);
        let drift_ns = age_ns.saturating_mul(drift_ppm) / 1_000_000;
        let error = self.window_ns.saturating_add(drift_ns);
        (monotonic, error.min(u32::MAX as u64) as u32)
    }

    /// Whether `later` can be the same clock as this one, still ticking.
    ///
    /// Two calibrations of one undisturbed pair of clocks may disagree by the
    /// slew ceiling over the time between them, plus both windows. More than
    /// that means the foreign clock was set rather than slewed, and every stamp
    /// converted across that moment is shifted by an amount nobody knows.
    pub fn is_continuous_with(&self, later: &Calibration, drift_ppm: u64) -> bool {
        let elapsed = later.monotonic_ns.saturating_sub(self.monotonic_ns);
        let allowed = elapsed.saturating_mul(drift_ppm) / 1_000_000
            + self.window_ns
            + later.window_ns
            // One nanosecond of tick granularity on each side, so a pair of
            // clocks that agree exactly is never called discontinuous.
            + 2;
        let moved = (later.offset_ns() - self.offset_ns()).unsigned_abs();
        moved <= allowed as u128
    }
}

fn read_clock(clock: libc::clockid_t) -> io::Result<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes a `timespec` we own.
    if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_realtime_stamp_converts_back_to_the_monotonic_moment_it_was_taken() {
        // Ground truth: the two clocks are read at almost the same instant, so
        // converting one must land within the calibration's own window of the
        // other. Nothing here trusts the conversion to check itself -- the
        // monotonic reading is taken independently.
        let calibration = Calibration::take(libc::CLOCK_REALTIME).expect("the realtime clock");
        let realtime = read_clock(libc::CLOCK_REALTIME).expect("the realtime clock");
        let monotonic = monotonic_ns();

        let (converted, error) = calibration.convert(realtime, MAX_SLEW_PPM);
        assert!(
            converted.abs_diff(monotonic) <= error as u64 + 1_000_000,
            "a stamp taken now converted to {converted}, but the monotonic clock says \
             {monotonic} (error claimed: {error} ns)"
        );
    }

    #[test]
    fn a_converted_stamp_never_claims_to_be_exact() {
        let calibration = Calibration::take(libc::CLOCK_REALTIME).expect("the realtime clock");
        let (_, error) = calibration.convert(calibration.foreign_ns, MAX_SLEW_PPM);
        assert!(
            error > 0,
            "a timestamp from another clock domain carries a non-zero ts_err_ns"
        );
    }

    #[test]
    fn the_error_grows_with_the_age_of_the_calibration() {
        let calibration = Calibration {
            foreign_ns: 1_000_000_000_000,
            monotonic_ns: 1_000_000_000,
            window_ns: 1_000,
        };
        let (_, fresh) = calibration.convert(1_000_000_000_000, MAX_SLEW_PPM);
        // A stamp one second before the calibration, at 500 ppm, is 500 us of
        // possible drift on top of the window.
        let (_, old) = calibration.convert(999_000_000_000, MAX_SLEW_PPM);
        assert_eq!(fresh, 1_000);
        assert_eq!(old, 1_000 + 500_000);
    }

    #[test]
    fn a_clock_that_was_stepped_is_not_continuous_with_what_it_was() {
        let first = Calibration {
            foreign_ns: 1_000_000_000_000,
            monotonic_ns: 1_000_000_000,
            window_ns: 1_000,
        };
        // A tenth of a second later, and the other clock advanced by a tenth of
        // a second: a pair of clocks doing nothing unusual.
        let ticking = Calibration {
            foreign_ns: 1_000_100_000_000,
            monotonic_ns: 1_100_000_000,
            window_ns: 1_000,
        };
        assert!(first.is_continuous_with(&ticking, MAX_SLEW_PPM));

        // The same tenth of a second, and the other clock jumped a second.
        let stepped = Calibration {
            foreign_ns: 1_001_100_000_000,
            monotonic_ns: 1_100_000_000,
            window_ns: 1_000,
        };
        assert!(!first.is_continuous_with(&stepped, MAX_SLEW_PPM));
    }
}
