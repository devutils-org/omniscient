//! Numbers as a person reads them, in as few cells as they fit: every view
//! formats with these, so one quantity reads the same on every screen.

/// A duration in nanoseconds: `850ns`, `12.4µs`, `3.20ms`, `1.50s`, `2m03s`.
pub fn duration(nanos: f64) -> String {
    if !nanos.is_finite() {
        return "?".to_owned();
    }
    let abs = nanos.abs();
    if abs < 1e3 {
        format!("{nanos:.0}ns")
    } else if abs < 1e6 {
        three(nanos / 1e3, "µs")
    } else if abs < 1e9 {
        three(nanos / 1e6, "ms")
    } else if abs < 60e9 {
        three(nanos / 1e9, "s")
    } else {
        let seconds = (nanos / 1e9).round() as i64;
        format!("{}m{:02}s", seconds / 60, seconds % 60)
    }
}

/// Three significant digits and a unit.
fn three(value: f64, unit: &str) -> String {
    let abs = value.abs();
    if abs >= 100.0 {
        format!("{value:.0}{unit}")
    } else if abs >= 10.0 {
        format!("{value:.1}{unit}")
    } else {
        format!("{value:.2}{unit}")
    }
}

/// A byte count in binary units: `512B`, `1.50KiB`, `3.20GiB`.
pub fn bytes(value: f64) -> String {
    if !value.is_finite() {
        return "?".to_owned();
    }
    let steps = [
        (1u64 << 50, "PiB"),
        (1 << 40, "TiB"),
        (1 << 30, "GiB"),
        (1 << 20, "MiB"),
        (1 << 10, "KiB"),
    ];
    for (size, unit) in steps {
        if value.abs() >= size as f64 {
            return three(value / size as f64, unit);
        }
    }
    format!("{value:.0}B")
}

/// A count with a decimal suffix: `999`, `1.20k`, `3.40M`.
pub fn count(value: f64) -> String {
    if !value.is_finite() {
        return "?".to_owned();
    }
    let abs = value.abs();
    if abs < 1e3 {
        if value.fract() == 0.0 {
            format!("{value:.0}")
        } else {
            format!("{value:.2}")
        }
    } else if abs < 1e6 {
        three(value / 1e3, "k")
    } else if abs < 1e9 {
        three(value / 1e6, "M")
    } else {
        three(value / 1e9, "G")
    }
}

/// A percentage: `0.42%`, `12.3%`, `100%`.
pub fn percent(value: f64) -> String {
    three(value, "%")
}

/// A quantity in the unit a result names (`ns`, `bytes`, `count`...), as
/// `omn`'s catalog spells units; anything else is a count followed by the
/// unit's name.
pub fn quantity(value: f64, unit: &str) -> String {
    match unit {
        "ns" => duration(value),
        "bytes" => bytes(value),
        "count" | "cycles" | "" => count(value),
        "ratio" => format!("{value:.3}"),
        "hz" => format!("{}Hz", count(value)),
        "joules" => format!("{}J", count(value)),
        "watts" => format!("{}W", count(value)),
        "celsius" => format!("{value:.1}°C"),
        other => format!("{} {other}", count(value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(850.0), "850ns");
        assert_eq!(duration(12_400.0), "12.4µs");
        assert_eq!(duration(3_200_000.0), "3.20ms");
        assert_eq!(duration(1.5e9), "1.50s");
        assert_eq!(duration(123e9), "2m03s");
    }

    #[test]
    fn bytes_and_counts() {
        assert_eq!(bytes(512.0), "512B");
        assert_eq!(bytes(1536.0), "1.50KiB");
        assert_eq!(count(999.0), "999");
        assert_eq!(count(1200.0), "1.20k");
        assert_eq!(percent(0.4242), "0.42%");
    }
}
