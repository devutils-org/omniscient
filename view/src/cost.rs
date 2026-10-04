//! What the view itself costs, measured, for the corner of the screen.
//!
//! `omn` shows its own price beside every measurement, and the view is no
//! exception: the cpu the drawing thread spent, as a share of the wall-clock
//! it has been running. Read from `CLOCK_THREAD_CPUTIME_ID`, so it is the
//! view's alone -- the collectors' cost is in the run's results, where every
//! collector's is.

/// The drawing thread's cpu, since it started.
#[derive(Debug, Clone, Copy)]
pub struct Meter {
    started_cpu_ns: u64,
    started_wall_ns: u64,
}

impl Default for Meter {
    fn default() -> Self {
        Self::start()
    }
}

impl Meter {
    /// Starts measuring the calling thread.
    pub fn start() -> Meter {
        Meter {
            started_cpu_ns: thread_cpu_ns(),
            started_wall_ns: monotonic_ns(),
        }
    }

    /// The thread's cpu since the start, in nanoseconds, and the wall-clock.
    pub fn spent(&self) -> (u64, u64) {
        (
            thread_cpu_ns().saturating_sub(self.started_cpu_ns),
            monotonic_ns().saturating_sub(self.started_wall_ns),
        )
    }

    /// The thread's cpu as a share of the wall-clock, in percent; `None`
    /// for the first half second, when the first drawing is all there is to
    /// divide and would read as a busy view.
    pub fn percent(&self) -> Option<f64> {
        let (cpu, wall) = self.spent();
        (wall >= 500_000_000).then(|| cpu as f64 * 100.0 / wall as f64)
    }
}

fn clock_ns(clock: libc::clockid_t) -> u64 {
    let mut spec = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `spec` is a valid timespec the call writes into.
    let status = unsafe { libc::clock_gettime(clock, &mut spec) };
    if status != 0 {
        return 0;
    }
    spec.tv_sec as u64 * 1_000_000_000 + spec.tv_nsec as u64
}

/// `CLOCK_THREAD_CPUTIME_ID`: the calling thread's cpu.
pub fn thread_cpu_ns() -> u64 {
    clock_ns(libc::CLOCK_THREAD_CPUTIME_ID)
}

/// `CLOCK_MONOTONIC`, the clock of every event.
pub fn monotonic_ns() -> u64 {
    clock_ns(libc::CLOCK_MONOTONIC)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sleeping_thread_costs_nearly_nothing() {
        let meter = Meter::start();
        assert_eq!(meter.percent(), None, "too early to say");
        std::thread::sleep(std::time::Duration::from_millis(600));
        let (cpu, wall) = meter.spent();
        assert!(wall >= 600_000_000, "wall {wall}");
        assert!(
            meter.percent().unwrap() < 20.0,
            "a sleep cost {cpu} ns of {wall}"
        );
    }

    #[test]
    fn a_busy_thread_costs_its_wall_clock() {
        let meter = Meter::start();
        let until = monotonic_ns() + 600_000_000;
        let mut spin = 0u64;
        while monotonic_ns() < until {
            spin = spin.wrapping_add(1);
        }
        std::hint::black_box(spin);
        let percent = meter.percent().unwrap();
        assert!(percent > 50.0, "{percent}");
    }
}
