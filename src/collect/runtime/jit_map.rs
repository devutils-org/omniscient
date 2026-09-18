//! `runtime.jit` from the symbol map a JIT publishes.
//!
//! A just-in-time compiler produces machine code that exists in no file, so a
//! profiler samples an address that belongs to nothing it can name. The
//! convention that fixes this is a plain text file, `/tmp/perf-<pid>.map`, one
//! line per generated code object:
//!
//! ```text
//! 7f41ff0ced8 2a JS:~Writable.write node:internal/streams/writable:504:36
//! ```
//!
//! Node writes it with `--perf-basic-prof`, the JVM with an agent, and several
//! other runtimes with a flag of their own. It is append-only, so reading the
//! bytes that appeared since the last read gives the code objects created since
//! the last read. That is this backend: the compilations, named, as they
//! happen.
//!
//! What it cannot give is how long a compilation took. The file records that
//! code exists, not when the compiler started or stopped, which is why this
//! backend is the fallback and the USDT probes are the first choice. A source
//! that timed compilation from this file would be inventing the number.
//!
//! The timestamps are honest about the same limit. A line is known to have
//! appeared between two reads and no more precisely than that, so every event
//! carries `ts_err_ns` equal to the gap between them. Lines already in the file
//! at the first read are older than this run by an amount the file does not
//! record, and they say so.

use std::path::PathBuf;
use std::time::Duration;

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::Target;

/// How often the map is read.
///
/// The file is appended to by the observed program and read here; the interval
/// is both the resolution of the timestamps and the cost of the source. Ten
/// milliseconds is the same cadence the heap shim is drained at, and reading a
/// few kilobytes that often is not a cost worth optimising.
const READ_INTERVAL: Duration = Duration::from_millis(10);

pub struct Prepare;

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let pid = target.first_pid().ok_or_else(|| {
            format!(
                "a JIT publishes its symbol map per process, and a {} target names none",
                target.kind()
            )
        })?;
        Ok(Box::new(JitMapCollector::new(pid)))
    }
}

pub struct JitMapCollector {
    id: CollectorId,
    pid: i32,
    path: PathBuf,
    /// How much of the file has been turned into events.
    consumed: u64,
    /// A line that arrived without its newline yet.
    partial: String,
    /// When the map was last read, which is what bounds the timestamps.
    last_read_ns: u64,
    read_once: bool,
    compilations: u64,
    /// Polls that found no map before the first one was ever read.
    ///
    /// Nothing is lost when a file that does not exist yet is not read: a
    /// runtime creates its map as it starts, and looking a few milliseconds
    /// early costs nothing. It is only when the map is never read at all that
    /// those polls mean something -- that this run watched a program and saw
    /// nothing, which is not the same as a program that compiled nothing.
    absent_before_first_read: u64,
    losses: Losses,
}

impl JitMapCollector {
    pub fn new(pid: i32) -> Self {
        Self {
            id: CollectorId::new("runtime.jit/fs:jit-map"),
            pid,
            path: PathBuf::from(format!("/tmp/perf-{pid}.map")),
            consumed: 0,
            partial: String::new(),
            last_read_ns: 0,
            read_once: false,
            compilations: 0,
            absent_before_first_read: 0,
            losses: Losses::default(),
        }
    }

    /// Reads whatever was appended since the last call.
    fn read_new(&mut self) -> Result<String, SampleError> {
        use std::io::{Read, Seek, SeekFrom};

        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.read_once {
                    // The map was there and is not any more: whatever the
                    // runtime wrote into it after the last read is gone.
                    self.losses.failed_reads += 1;
                    self.losses.note(format!(
                        "{} disappeared while the program was still running",
                        self.path.display()
                    ));
                } else {
                    self.absent_before_first_read += 1;
                }
                return Ok(String::new());
            }
            // A map that exists and cannot be read is a different problem, and
            // says so: it belongs to another user, and reading it needs their
            // privileges rather than a differently started program.
            Err(error) => {
                return Err(SampleError::Transient(format!(
                    "{}: {error}",
                    self.path.display()
                )))
            }
        };

        let length = file
            .metadata()
            .map_err(|error| SampleError::Transient(format!("{}: {error}", self.path.display())))?
            .len();

        if length < self.consumed {
            // The file is append-only by convention. Shorter than what was
            // already read means it was replaced -- a new process with a
            // recycled pid, or a runtime that restarted its map -- and whatever
            // stood between the two reads is gone.
            self.losses.dropped += 1;
            self.losses.note(format!(
                "{} shrank from {} to {length} bytes: it was replaced, and the entries \
                 written before that are not recoverable",
                self.path.display(),
                self.consumed
            ));
            self.consumed = 0;
            self.partial.clear();
        }

        if length == self.consumed {
            return Ok(String::new());
        }

        file.seek(SeekFrom::Start(self.consumed))
            .map_err(|error| SampleError::Transient(format!("{}: {error}", self.path.display())))?;
        let mut appended = String::new();
        let read = file
            .read_to_string(&mut appended)
            .map_err(|error| SampleError::Transient(format!("{}: {error}", self.path.display())))?;
        self.consumed += read as u64;
        Ok(appended)
    }

    /// Turns the appended text into one event per code object.
    fn decode(&mut self, appended: &str, seq: u64) -> Vec<Event> {
        self.partial.push_str(appended);
        // A line without its newline is a line the runtime is still writing.
        let complete = match self.partial.rfind('\n') {
            Some(last) => self.partial.drain(..=last).collect::<String>(),
            None => return Vec::new(),
        };

        let now_ns = monotonic_ns();
        let first_read = !self.read_once;
        self.read_once = true;
        // A line appeared between the previous read and this one, and the file
        // says nothing more precise. The first read is the exception: those
        // lines are older than the run by an amount nothing records, so they
        // carry the largest error the model can express rather than a small one
        // that would be a lie.
        let ts_err_ns = if first_read {
            u32::MAX
        } else {
            now_ns
                .saturating_sub(self.last_read_ns)
                .try_into()
                .unwrap_or(u32::MAX)
        };
        self.last_read_ns = now_ns;

        let mut index = seq;
        let mut events = Vec::new();
        for line in complete.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Some(entry) = Entry::parse(line) else {
                // A line nobody can decode is a gap in the data, not a line to
                // skip quietly: a map in a format this does not know would
                // otherwise read as a program that compiled nothing.
                self.losses.undecoded += 1;
                self.losses.note(format!(
                    "a line of {} is not `<hex address> <hex size> <symbol>`",
                    self.path.display()
                ));
                continue;
            };
            self.compilations += 1;
            events.push(Event {
                ts_ns: now_ns,
                ts_err_ns,
                kind: "runtime.jit".to_owned(),
                keys: vec![KeyValue::task(self.pid)],
                attrs: vec![
                    Attr::uint("count", 1),
                    Attr::uint("bytes", entry.size),
                    Attr::uint("addr", entry.address),
                    Attr::text("method", entry.symbol),
                    // `Builtin:` entries are the runtime's own precompiled
                    // code, published through the same file. They are not
                    // compilations this run caused, and a user counting
                    // compilations has to be able to tell them apart.
                    Attr::text("kind", entry.kind),
                    Attr::uint("first_read", u64::from(first_read)),
                ],
                collector_id: self.id.clone(),
                seq: index,
            });
            index += 1;
        }
        events
    }
}

impl Collector for JitMapCollector {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "count"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let appended = self.read_new()?;
        Ok(self.decode(&appended, seq))
    }

    fn stop(&mut self) -> Vec<Event> {
        // Code compiled just before the program exited is in the file; the
        // program is gone, the file is not.
        let appended = self.read_new().unwrap_or_default();
        let trailing = self.decode(&appended, u64::MAX / 2);

        if self.compilations == 0 {
            // Every poll of this run looked and found nothing. That is what the
            // run has to report: not zero compilations, but zero observations.
            self.losses.failed_reads += self.absent_before_first_read;
            self.losses.note(format!(
                "{} does not exist: nothing was read for pid {}, because the program either \
                 has no JIT or was not started so that it writes one \
                 (node --perf-basic-prof, or a JVM agent that maintains it). That is not the \
                 same as a program that compiled nothing",
                self.path.display(),
                self.pid
            ));
        }
        trailing
    }

    fn tick(&self) -> Option<Duration> {
        Some(READ_INTERVAL)
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

/// The kinds V8 writes in front of a symbol.
///
/// Anything else is reported as `code`: the file is a convention, not a format,
/// and the only taxonomy this collector can pass on is one it was told.
const V8_KINDS: &[&str] = &[
    "js",
    "lazycompile",
    "builtin",
    "stub",
    "regexp",
    "script",
    "eval",
    "code",
    "interpreter",
    "handler",
    "bytecodehandler",
];

/// One line of the map.
struct Entry {
    address: u64,
    size: u64,
    symbol: String,
    kind: String,
}

impl Entry {
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.splitn(3, ' ');
        let address = u64::from_str_radix(fields.next()?.trim(), 16).ok()?;
        let size = u64::from_str_radix(fields.next()?.trim(), 16).ok()?;
        let symbol = fields.next()?.trim();
        if symbol.is_empty() {
            return None;
        }
        // V8 puts what kind of code it is before the first colon: `JS:~name
        // file:line`, `Builtin:Name`, `RegExp:pattern`. Only the kinds it is
        // known to write are read as kinds -- a JVM agent writes
        // `com.example.Service::handle`, whose colon is part of the name, and
        // inventing a kind called `com.example.service` from it would be a
        // taxonomy this tool made up.
        let kind = symbol
            .split_once(':')
            .map(|(prefix, _)| prefix.to_ascii_lowercase())
            .filter(|prefix| V8_KINDS.contains(&prefix.as_str()))
            .unwrap_or_else(|| "code".to_owned());
        Some(Self {
            address,
            size,
            symbol: symbol.to_owned(),
            kind,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collector() -> JitMapCollector {
        JitMapCollector::new(std::process::id() as i32)
    }

    #[test]
    fn a_line_names_the_code_and_what_kind_of_code_it_is() {
        let entry = Entry::parse("7f41ff0ced8 2a JS:~Writable.write node:internal:504:36")
            .expect("a well-formed line");
        assert_eq!(entry.address, 0x7f41ff0ced8);
        assert_eq!(entry.size, 0x2a);
        assert_eq!(entry.kind, "js");
        assert!(entry.symbol.starts_with("JS:~Writable.write"));

        assert_eq!(
            Entry::parse("56310a2c0000 300 Builtin:DeoptimizationEntry_Eager")
                .unwrap()
                .kind,
            "builtin"
        );
        // A JVM agent writes bare method names, with no kind in front.
        assert_eq!(
            Entry::parse("7f0000 10 com.example.Service::handle")
                .unwrap()
                .kind,
            "code",
            "a symbol whose colon is part of the name is not a kind"
        );
    }

    #[test]
    fn a_line_that_is_not_an_entry_is_counted_rather_than_skipped() {
        assert!(Entry::parse("this is not a map line").is_none());
        assert!(Entry::parse("7f0000 10").is_none(), "no symbol");
        assert!(Entry::parse("").is_none());

        let mut collector = collector();
        let events = collector.decode("nonsense\n7f0000 10 JS:~ok\n", 0);
        assert_eq!(events.len(), 1);
        assert_eq!(collector.losses().undecoded, 1);
        assert!(!collector.losses().is_clean());
    }

    /// The runtime appends to the map while this reads it, so the last line of
    /// a read is regularly half-written. Half a line is not an entry yet.
    #[test]
    fn a_line_still_being_written_waits_for_its_newline() {
        let mut collector = collector();
        assert!(collector.decode("7f0000 10 JS:~half", 0).is_empty());
        let events = collector.decode("way\n", 0);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].attr("method").unwrap().to_string(), "JS:~halfway");
    }

    #[test]
    fn the_first_read_is_marked_and_the_later_ones_are_bounded_by_the_gap() {
        let mut collector = collector();
        let first = collector.decode("7f0000 10 JS:~a\n", 0);
        assert_eq!(first[0].attr("first_read").unwrap().as_f64(), Some(1.0));
        assert_eq!(
            first[0].ts_err_ns,
            u32::MAX,
            "how long before the run it was compiled is not in the file"
        );

        let later = collector.decode("7f1000 10 JS:~b\n", 1);
        assert_eq!(later[0].attr("first_read").unwrap().as_f64(), Some(0.0));
        assert!(
            later[0].ts_err_ns > 0 && later[0].ts_err_ns < u32::MAX,
            "an entry between two reads is bounded by their gap, got {}",
            later[0].ts_err_ns
        );
    }

    #[test]
    fn a_target_without_a_process_is_refused_by_name() {
        let error = Box::new(Prepare)
            .attach(&Target::System)
            .err()
            .expect("the machine as a whole publishes no map");
        assert!(error.contains("names none"), "{error}");
    }
}
