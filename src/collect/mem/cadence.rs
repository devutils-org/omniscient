//! When a polling collector publishes its next value.
//!
//! A run ticks as fast as its fastest collector needs: a shim pipe has to be
//! drained every ten milliseconds or it overflows, and every other collector in
//! that session is then asked for a sample just as often. Publishing on that
//! tick would hand back a series ten times denser than the request asked for,
//! which is not a loss but is not the measurement that was requested either.
//!
//! So reading and publishing are separate. The collector reads whenever it is
//! asked, and publishes on the grid the request named.

use std::time::Duration;

pub struct Cadence {
    interval_ns: u64,
    /// When the next value is due, or `None` until the first call anchors the
    /// grid at the start of the run.
    due_ns: Option<u64>,
}

impl Cadence {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval_ns: interval.as_nanos() as u64,
            due_ns: None,
        }
    }

    /// Whether a value is due now, advancing the grid when it is.
    ///
    /// The grid is absolute rather than relative to the last publication, so a
    /// tick that arrives late does not push every later one back with it. A
    /// collector that has fallen behind by more than one interval restarts from
    /// now rather than publishing a burst to catch up.
    pub fn due(&mut self, now_ns: u64) -> bool {
        // The first sample publishes, and it is what the grid is anchored on: a
        // series that started one interval late would have no baseline to read
        // the rest against, and a grid anchored anywhere else is a grid in a
        // clock domain the run does not own. `CLOCK_MONOTONIC` counts from
        // boot, so anchoring at zero puts the next slot an arbitrary fraction
        // of an interval away -- with an interval of an hour and an uptime of
        // fifty-eight minutes, every tick of the run falls inside the slack
        // below and the cadence publishes on every one of them.
        let Some(due_ns) = self.due_ns else {
            self.due_ns = Some(now_ns.saturating_add(self.interval_ns));
            return true;
        };
        // Within a tenth of the interval of being due counts as due: the run's
        // tick is coarser than the grid, and a point that missed its slot by a
        // millisecond would otherwise wait a whole interval for the next one.
        if now_ns.saturating_add(self.interval_ns / 10) < due_ns {
            return false;
        }
        let next_ns = due_ns.saturating_add(self.interval_ns);
        self.due_ns = Some(if next_ns <= now_ns {
            now_ns.saturating_add(self.interval_ns)
        } else {
            next_ns
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_value_is_always_due() {
        let mut cadence = Cadence::new(Duration::from_millis(50));
        assert!(cadence.due(0));
        assert!(!cadence.due(1));
    }

    /// The run ticks every 10 ms and the request asked for 50 ms, so four ticks
    /// out of five publish nothing.
    #[test]
    fn a_faster_tick_than_the_request_does_not_publish_on_every_tick() {
        const MS: u64 = 1_000_000;
        let mut cadence = Cadence::new(Duration::from_millis(50));
        let published: Vec<u64> = (0..=100)
            .map(|tick| tick * 10 * MS)
            .filter(|&now| cadence.due(now))
            .collect();

        assert_eq!(
            published.len(),
            21,
            "a second of 50 ms points, plus the first"
        );
        assert_eq!(published[1], 50 * MS);
        assert_eq!(published[2], 100 * MS);
    }

    /// The grid belongs to the run, not to the machine. `CLOCK_MONOTONIC`
    /// counts from boot, and a grid anchored at zero is a grid whose next slot
    /// depends on how long the machine has been up: at an uptime of 58 minutes
    /// the next hourly slot is two minutes away, inside the six-minute slack,
    /// so every sample of the run reads as due. That made two tests of
    /// `mem.heap.live` pass or fail on uptime alone.
    #[test]
    fn the_grid_is_anchored_on_the_first_sample_rather_than_on_boot() {
        const SECOND: u64 = 1_000_000_000;
        let mut cadence = Cadence::new(Duration::from_secs(3600));

        // An uptime of 58 minutes: shorter than the interval, so no slot has
        // gone by since boot.
        let start = 3483 * SECOND;
        assert!(cadence.due(start), "the first sample publishes");
        assert!(
            !cadence.due(start + 1),
            "an hour has not passed since the first sample"
        );
        assert!(!cadence.due(start + 3000 * SECOND));
        assert!(
            cadence.due(start + 3600 * SECOND),
            "an hour later, it is due"
        );
    }

    /// A tick that arrives late must not shift every later point: the series
    /// keeps the spacing it claims.
    #[test]
    fn a_late_tick_does_not_move_the_grid() {
        const MS: u64 = 1_000_000;
        let mut cadence = Cadence::new(Duration::from_millis(50));
        assert!(cadence.due(0));
        // The sample that was due at 50 ms arrives at 58 ms.
        assert!(cadence.due(58 * MS));
        // The next one is still due at 100, not at 108.
        assert!(!cadence.due(90 * MS));
        assert!(cadence.due(100 * MS));
    }
}
