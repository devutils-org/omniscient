//! The clock.
//!
//! One domain, `CLOCK_MONOTONIC`, everywhere. This is not a preference: an event
//! read from procfs and an event produced by an eBPF program have to be
//! comparable, and `bpf_ktime_get_ns()` is `CLOCK_MONOTONIC`. Using
//! `CLOCK_REALTIME` here, or `bpf_ktime_get_boot_ns()` there, would silently
//! shift half the events by the machine's suspend time and destroy every join
//! without producing a single visible error.
//!
//! Timestamps taken elsewhere -- a NIC's PHC, a GPU's own counter -- are in
//! another domain. They are converted at the boundary and carry a non-zero
//! `ts_err_ns` saying how much the conversion could be off.

/// Reads `CLOCK_MONOTONIC`, in nanoseconds.
///
/// This is the same timebase an eBPF program sees through
/// `bpf_ktime_get_ns()`, so events from both sides sort into one stream.
pub fn monotonic_ns() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `clock_gettime` writes a `timespec` we own, and `CLOCK_MONOTONIC`
    // is available on every kernel this tool targets.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) };
    debug_assert_eq!(result, 0, "CLOCK_MONOTONIC must be readable");
    (time.tv_sec as u64) * 1_000_000_000 + (time.tv_nsec as u64)
}

/// One reading of another clock between two reads of `CLOCK_MONOTONIC`: the
/// other clock's value belongs somewhere in `[before, after]`, and nothing
/// says where, so the midpoint is taken and half the width is its error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bracket {
    pub before: u64,
    pub other: u64,
    pub after: u64,
}

impl Bracket {
    fn midpoint(&self) -> f64 {
        (self.before as f64 + self.after as f64) / 2.0
    }

    fn half_width(&self) -> f64 {
        self.after.saturating_sub(self.before) as f64 / 2.0
    }
}

/// The map from a device's clock to `CLOCK_MONOTONIC`, measured.
///
/// A GPU tracer's timestamps are in its own domain -- CUPTI's is a wall clock
/// of its own choosing, not `CLOCK_MONOTONIC` -- so they are converted at the
/// boundary, by brackets taken in the traced process as the run starts, at
/// every drain, and as it ends. The map is the line through the first and the
/// last bracket's midpoints, so the drift between the two clocks over the run
/// is in its slope rather than in its error; the brackets in between are what
/// the line is checked against.
///
/// **The error is measured, never declared.** It is the widest half-bracket
/// (how precisely any one pair could be placed) plus the furthest any bracket
/// sits from the line (how far the two clocks departed from a straight line
/// between the ends). It is at least a nanosecond, because a converted
/// timestamp is never exact, and `ts_err_ns` zero would say it was.
///
/// **Outside the brackets the error grows.** A timestamp later than the last
/// bracket -- a record that arrived before the bracket that would have
/// enclosed it -- is extrapolated along the line, and how wrong the line's
/// slope may be is measured too: the two ends' half-widths and the furthest
/// departure, over the span the brackets cover. Before a second bracket
/// there is no slope to measure, and [`UNMEASURED_RATE`] stands in for it,
/// declared rather than measured, and wide on purpose.
///
/// **What it does not cover.** It bounds the conversion of the other clock's
/// value into `CLOCK_MONOTONIC`, and nothing upstream of it: how the vendor
/// placed the device's work on its own clock is not something a bracket taken
/// on the host can see (`docs/families/gpu.md`, the `gpu.kernel` section, says
/// what that adds on this machine).
#[derive(Debug, Clone, Default)]
pub struct Calibration {
    brackets: Vec<Bracket>,
}

/// What a calibration amounts to at one moment: the line, and how far from
/// it a converted timestamp may be.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClockMap {
    /// `CLOCK_MONOTONIC` at the other clock's `origin`.
    pub at_origin: f64,
    pub origin: u64,
    /// Monotonic nanoseconds per nanosecond of the other clock.
    pub slope: f64,
    /// The error inside the brackets.
    pub err_ns: u64,
    /// The other clock's first and last bracketed values.
    pub first: u64,
    pub last: u64,
    /// How wrong the slope may be, as a fraction: what an extrapolated
    /// timestamp's error grows by, per nanosecond outside the brackets.
    pub rate_err: f64,
}

/// The slope error assumed before a second bracket measures one: 500 ppm,
/// the most the kernel slews `CLOCK_MONOTONIC`, and an order of magnitude
/// past a quartz oscillator's tolerance.
pub const UNMEASURED_RATE: f64 = 500e-6;

impl ClockMap {
    /// The monotonic time of a timestamp of the other clock.
    pub fn to_monotonic(&self, other: u64) -> u64 {
        let delta = other as f64 - self.origin as f64;
        (self.at_origin + delta * self.slope).round().max(0.0) as u64
    }

    /// The error of one converted timestamp: the brackets' error, plus what
    /// extrapolating to it outside them may add.
    pub fn err_at(&self, other: u64) -> u64 {
        let outside = if other > self.last {
            other - self.last
        } else {
            self.first.saturating_sub(other)
        };
        self.err_ns
            .saturating_add((outside as f64 * self.rate_err).ceil() as u64)
    }

    /// A span of the other clock, in monotonic nanoseconds.
    pub fn span(&self, start: u64, end: u64) -> u64 {
        (end.saturating_sub(start) as f64 * self.slope).round() as u64
    }
}

impl Calibration {
    pub fn push(&mut self, bracket: Bracket) {
        if bracket.after >= bracket.before {
            self.brackets.push(bracket);
        }
    }

    pub fn len(&self) -> usize {
        self.brackets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.brackets.is_empty()
    }

    /// The map the brackets so far give, or `None` before the first one.
    ///
    /// With a single bracket the slope is taken as one: the clocks are
    /// assumed to run at one rate until a second bracket says otherwise, and
    /// that assumption is what the next bracket's residual measures.
    pub fn map(&self) -> Option<ClockMap> {
        let first = *self.brackets.first()?;
        let last = *self.brackets.last()?;
        let span_other = last.other as f64 - first.other as f64;
        let slope = if self.brackets.len() >= 2 && span_other > 0.0 {
            (last.midpoint() - first.midpoint()) / span_other
        } else {
            1.0
        };
        let mut map = ClockMap {
            at_origin: first.midpoint(),
            origin: first.other,
            slope,
            err_ns: 1,
            first: first.other,
            last: last.other,
            rate_err: UNMEASURED_RATE,
        };
        let mut widest = 0.0f64;
        let mut furthest = 0.0f64;
        for bracket in &self.brackets {
            widest = widest.max(bracket.half_width());
            let predicted = map.at_origin + (bracket.other as f64 - first.other as f64) * slope;
            furthest = furthest.max((predicted - bracket.midpoint()).abs());
        }
        map.err_ns = ((widest + furthest).ceil() as u64).max(1);
        if self.brackets.len() >= 2 && span_other > 0.0 {
            map.rate_err = (first.half_width() + last.half_width() + furthest) / span_other;
        }
        Some(map)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bracket(before: u64, other: u64, after: u64) -> Bracket {
        Bracket {
            before,
            other,
            after,
        }
    }

    #[test]
    fn one_bracket_maps_by_its_offset_and_errs_by_half_its_width() {
        let mut calibration = Calibration::default();
        assert!(calibration.map().is_none());
        calibration.push(bracket(1_000, 5_000_000, 1_100));
        let map = calibration.map().unwrap();
        assert_eq!(map.to_monotonic(5_000_000), 1_050);
        assert_eq!(map.to_monotonic(5_000_500), 1_550);
        assert_eq!(map.err_ns, 50);
    }

    /// Two clocks drifting apart by 100 ppm: the line through the ends takes
    /// the drift as its slope, and a bracket in between that sits on the line
    /// adds nothing to the error.
    #[test]
    fn drift_goes_into_the_slope_and_not_into_the_error() {
        let mut calibration = Calibration::default();
        calibration.push(bracket(0, 1_000_000_000, 20));
        calibration.push(bracket(500_050_000, 1_500_000_000, 500_050_020));
        calibration.push(bracket(1_000_100_000, 2_000_000_000, 1_000_100_020));
        let map = calibration.map().unwrap();
        assert!((map.slope - 1.0001).abs() < 1e-9, "{}", map.slope);
        assert_eq!(map.to_monotonic(2_000_000_000), 1_000_100_010);
        assert_eq!(map.span(1_000_000_000, 1_000_001_000), 1_000);
        assert_eq!(map.err_ns, 10);
    }

    /// A clock that jumped in the middle of the run -- a wall clock stepped
    /// by NTP -- is a bracket off the line, and the error says by how much.
    #[test]
    fn a_bracket_off_the_line_is_the_error() {
        let mut calibration = Calibration::default();
        calibration.push(bracket(0, 1_000, 10));
        calibration.push(bracket(1_000_000, 1_000_000 + 1_000 + 7_000, 1_000_010));
        calibration.push(bracket(2_000_000, 2_001_000, 2_000_010));
        let map = calibration.map().unwrap();
        assert_eq!(map.err_ns, 5 + 7_000);
    }

    /// A timestamp past the last bracket is extrapolated, and its error grows
    /// with the distance, by what the brackets say of the slope -- or by the
    /// declared bound before there are two of them.
    #[test]
    fn a_timestamp_outside_the_brackets_carries_the_extrapolation() {
        let mut calibration = Calibration::default();
        calibration.push(bracket(0, 1_000_000, 100));
        let one = calibration.map().unwrap();
        assert_eq!(one.err_at(1_000_000), 50);
        assert_eq!(one.err_at(11_000_000), 50 + 5_000, "500 ppm of 10 ms");
        calibration.push(bracket(1_000_000_000, 1_001_000_000, 1_000_000_100));
        let two = calibration.map().unwrap();
        assert_eq!(two.err_at(500_000_000), two.err_ns, "inside: no growth");
        // Half-widths 50 + 50 over a second of the other clock: 100 ppb,
        // so 1 s past the last bracket adds 100 ns.
        assert_eq!(two.err_at(2_001_000_000), two.err_ns + 100);
        assert_eq!(two.err_at(0), two.err_ns + 1);
    }

    #[test]
    fn an_exact_conversion_still_carries_a_nanosecond() {
        let mut calibration = Calibration::default();
        calibration.push(bracket(100, 100, 100));
        assert_eq!(calibration.map().unwrap().err_ns, 1);
    }

    #[test]
    fn a_bracket_whose_ends_are_reversed_is_not_kept() {
        let mut calibration = Calibration::default();
        calibration.push(bracket(100, 7, 90));
        assert!(calibration.is_empty());
    }

    #[test]
    fn the_monotonic_clock_moves_forward() {
        let first = monotonic_ns();
        let second = monotonic_ns();
        assert!(second >= first, "CLOCK_MONOTONIC went backwards");
        assert!(
            first > 0,
            "CLOCK_MONOTONIC should be past zero on a running machine"
        );
    }
}
