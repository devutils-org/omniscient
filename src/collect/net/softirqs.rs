//! The kernel's own softirq table, read out of `/proc/softirqs`.
//!
//! Two things come from this file, and neither is written down anywhere in this
//! build.
//!
//! **Which number is `NET_RX`.** The vector number a `softirq_entry` tracepoint
//! reports is an index into the kernel's `softirq_to_name` table, and this file
//! is that table printed in that order: one row per vector, labelled, from
//! index zero. So the number the eBPF program compares against is read off the
//! running kernel rather than assumed, which is the same discipline the field
//! offsets in `src/bpf/btf.rs` are read with and for the same reason -- a
//! vector number baked in at the wrong value measures the timer softirq and
//! calls it network time.
//!
//! **How often a handler ran.** The counts are per cpu and since boot, which
//! makes them an independent check on the eBPF source: over a window that
//! strictly contains a run, the run cannot have seen more `NET_RX` handlers
//! than this file says happened. `tests/scenario_net.rs` uses exactly that.

use std::collections::BTreeMap;

const SOFTIRQS: &str = "/proc/softirqs";

/// The softirq vectors this kernel has, in its own order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vectors {
    /// Vector names by their number, which is the row's position in the file.
    names: Vec<String>,
    /// How many times each vector's handler has run, summed over cpus.
    ///
    /// Per-cpu in the file; summed here because every question this build asks
    /// of it is machine-wide, and a per-cpu breakdown that nothing reads is a
    /// column of numbers to keep correct for no reason.
    runs: BTreeMap<u32, u64>,
}

impl Vectors {
    /// Reads the table as it stands now.
    pub fn read() -> Result<Self, String> {
        let text = std::fs::read_to_string(SOFTIRQS)
            .map_err(|error| format!("cannot read {SOFTIRQS}: {error}"))?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self, String> {
        let mut names = Vec::new();
        let mut runs = BTreeMap::new();
        // The first line is the cpu header. Every line after it is a vector,
        // in vector order, and its position is the number the tracepoint
        // reports.
        for line in text.lines().skip(1) {
            let mut fields = line.split_whitespace();
            let Some(label) = fields.next() else { continue };
            let name = label.trim_end_matches(':').to_owned();
            if name.is_empty() {
                continue;
            }
            let total = fields.filter_map(|count| count.parse::<u64>().ok()).sum();
            runs.insert(names.len() as u32, total);
            names.push(name);
        }
        if names.is_empty() {
            return Err(format!(
                "{SOFTIRQS} named no softirq vector, so there is no way to tell which number is \
                 the network one"
            ));
        }
        Ok(Self { names, runs })
    }

    /// The number of the vector with this name, as this kernel orders them.
    pub fn number_of(&self, name: &str) -> Option<u32> {
        self.names
            .iter()
            .position(|known| known == name)
            .map(|at| at as u32)
    }

    /// The name of a vector number, for an event a person has to read.
    pub fn name_of(&self, number: u32) -> Option<&str> {
        self.names.get(number as usize).map(String::as_str)
    }

    /// How many vectors this kernel has, which is how many rows the file had.
    pub fn count(&self) -> usize {
        self.names.len()
    }

    /// How many times this vector's handler has run since boot, over all cpus.
    pub fn runs(&self, number: u32) -> Option<u64> {
        self.runs.get(&number).copied()
    }

    /// The vectors a network source measures, and the names it did not find.
    ///
    /// Returned together because a kernel without `NET_RX` is not a kernel to
    /// guess about: the caller refuses and says which name was missing, rather
    /// than measuring the vectors it did find and calling the answer network
    /// time.
    pub fn network(&self) -> (Vec<u32>, Vec<&'static str>) {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        for name in NETWORK_VECTORS {
            match self.number_of(name) {
                Some(number) => found.push(number),
                None => missing.push(name),
            }
        }
        (found, missing)
    }
}

/// The two vectors the network stack runs in.
///
/// `NET_RX` is where a received packet is processed, and it is the one that
/// matters; `NET_TX` is the completion side and is usually small. Named rather
/// than numbered, because the names are what the kernel prints and the numbers
/// are what it happens to have assigned.
pub const NETWORK_VECTORS: [&str; 2] = ["NET_TX", "NET_RX"];

#[cfg(test)]
mod tests {
    use super::*;

    /// Two cpus and four vectors, laid out the way the kernel lays them out.
    const SAMPLE: &str = "                    CPU0       CPU1\n\
                          \x20         HI:          4          1\n\
                          \x20      TIMER:     205159     114992\n\
                          \x20     NET_TX:         17          3\n\
                          \x20     NET_RX:       5000       6000\n";

    /// A vector's number is its row's position, because that is what the
    /// kernel's own table is. Reading it any other way -- by name from a list
    /// written here -- is the baked-in constant this file exists to avoid.
    #[test]
    fn a_vectors_number_is_its_position_in_the_file() {
        let vectors = Vectors::parse(SAMPLE).expect("a table");
        assert_eq!(vectors.number_of("HI"), Some(0));
        assert_eq!(vectors.number_of("NET_TX"), Some(2));
        assert_eq!(vectors.number_of("NET_RX"), Some(3));
        assert_eq!(vectors.number_of("BLOCK"), None);
        assert_eq!(vectors.name_of(3), Some("NET_RX"));
        assert_eq!(vectors.name_of(9), None);
    }

    /// The counts are per cpu and a machine-wide question wants their sum.
    #[test]
    fn a_vectors_runs_are_summed_over_the_cpus_that_ran_it() {
        let vectors = Vectors::parse(SAMPLE).expect("a table");
        assert_eq!(vectors.runs(3), Some(11_000));
        assert_eq!(vectors.runs(2), Some(20));
    }

    /// A kernel that does not name `NET_RX` is refused rather than measured
    /// with whatever it does name.
    #[test]
    fn a_missing_network_vector_is_named_rather_than_skipped() {
        let vectors = Vectors::parse(
            "                    CPU0\n          HI:          4\n       TIMER:          1\n",
        )
        .expect("a table");
        let (found, missing) = vectors.network();
        assert!(found.is_empty());
        assert_eq!(missing, vec!["NET_TX", "NET_RX"]);
    }

    /// A file with nothing but its header says so, because the alternative is
    /// a program that compares a vector number against nothing.
    #[test]
    fn a_table_with_no_vectors_is_an_error() {
        assert!(Vectors::parse("      CPU0\n").is_err());
    }

    /// The real file, on this machine, has the vectors this build asks for and
    /// counts that only grow.
    #[test]
    fn this_kernel_names_both_network_vectors() {
        let Ok(vectors) = Vectors::read() else {
            eprintln!("skipped: this machine has no {SOFTIRQS}");
            return;
        };
        let (found, missing) = vectors.network();
        assert!(missing.is_empty(), "this kernel does not name {missing:?}");
        assert_eq!(found.len(), 2);
        let rx = vectors
            .number_of("NET_RX")
            .expect("this kernel names NET_RX");
        let before = vectors.runs(rx).expect("a count for NET_RX");
        let after = Vectors::read()
            .expect("a second read")
            .runs(rx)
            .expect("a count for NET_RX");
        assert!(
            after >= before,
            "a since-boot counter went backwards: {before} then {after}"
        );
    }
}
