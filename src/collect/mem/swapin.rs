//! `mem.swap` per process: the reads from swap a program's own threads made.
//!
//! The kernel counts swap traffic for the machine and for a cgroup, never for
//! a process. What it does per process is run the function: every read of a
//! folio from a swap device -- the fault that needed it, and the readahead
//! that fault started -- goes through `swap_read_folio`, on the thread that
//! faulted. A `kprobe` counting event on that function, opened on the
//! program's threads and inherited by what they start, is therefore a count
//! of that program's swap-ins, made by the kernel's own perf counter and with
//! no program loaded into it at all.
//!
//! What it does not have is the size: a folio is one page in the common case
//! and more where the kernel swaps large folios in whole. So this backend
//! counts reads, and says so; the flow in bytes stays with the cgroup and
//! machine backends.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::clock::monotonic_ns;
use crate::collect::{Collector, Losses, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event, KeyValue};
use crate::request::{Mode, Target};
use perf_event_open_sys as sys;

/// The kernel function every read from swap passes through, since 6.10.
const SWAP_READ: &[&str] = &["swap_read_folio", "swap_readpage"];

pub struct Prepare(Mode);

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Count => Ok(Self(mode)),
            other => Err(format!(
                "a process's swap-ins are counted by a kernel counter: `{}` cannot be served \
                 from it; poll it or count it over the window",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        let (tids, pid) = match target {
            Target::Pid(pids) => {
                let mut tids = Vec::new();
                for pid in pids {
                    tids.extend(crate::perf::threads_of(*pid));
                }
                (tids, pids.first().copied())
            }
            Target::Tid(tids) => (tids.clone(), None),
            other => {
                return Err(format!(
                    "a swap-in is counted per process here, and a {} target names none: the \
                     machine's flow is `mem.swap --system`, a cgroup's is its memory.stat",
                    other.kind()
                ))
            }
        };
        let symbol = swap_read_symbol()?;
        let mut losses = Losses::default();
        if let Some(kib) = largest_small_folio_kib() {
            losses.note(format!(
                "this kernel builds anonymous folios of up to {kib} KiB \
                 (/sys/kernel/mm/transparent_hugepage/hugepages-{kib}kB), and swaps such a \
                 folio in with one read: `swapins` counts reads, and `in_bytes`, one page a \
                 read, may be short by up to {} times; the cgroup's memory.stat counts pages",
                kib * 1024 / page_size()
            ));
        }
        Ok(Box::new(SwapIn {
            id: CollectorId::new("mem.swap/perf:kprobe"),
            mode: self.0,
            symbol,
            tids,
            key: pid.map(KeyValue::task),
            counters: Vec::new(),
            // SAFETY: sysconf reads a constant of the running system.
            page_size: page_size(),
            previous: None,
            opened: None,
            losses,
        }))
    }
}

fn page_size() -> u64 {
    // SAFETY: sysconf reads a constant of the running system.
    match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        size if size > 0 => size as u64,
        _ => 4096,
    }
}

/// The largest multi-page anonymous folio this kernel may build below the
/// PMD size, in KiB, or `None` where every one of them is turned off.
///
/// Each size has its own switch, `hugepages-<n>kB/enabled`: `always` and
/// `madvise` turn it on, `inherit` takes the top-level `enabled`, and
/// `never` turns it off. The PMD size is a huge page, which swap splits.
fn largest_small_folio_kib() -> Option<u64> {
    let root = std::path::Path::new("/sys/kernel/mm/transparent_hugepage");
    let chosen = |text: &str| {
        text.split_whitespace()
            .find_map(|word| word.strip_prefix('[')?.strip_suffix(']').map(str::to_owned))
    };
    let top = chosen(&std::fs::read_to_string(root.join("enabled")).ok()?)?;
    let pmd_kib = std::fs::read_to_string(root.join("hpage_pmd_size"))
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?
        / 1024;
    std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let kib: u64 = name
                .strip_prefix("hugepages-")?
                .strip_suffix("kB")?
                .parse()
                .ok()?;
            let mode = chosen(&std::fs::read_to_string(entry.path().join("enabled")).ok()?)?;
            let on = match mode.as_str() {
                "always" | "madvise" => true,
                "inherit" => top != "never",
                _ => false,
            };
            (on && kib < pmd_kib).then_some(kib)
        })
        .max()
}

/// The first of [`SWAP_READ`] this kernel has.
fn swap_read_symbol() -> Result<&'static str, String> {
    let symbols = std::fs::read_to_string("/proc/kallsyms")
        .map_err(|error| format!("cannot read /proc/kallsyms: {error}"))?;
    SWAP_READ
        .iter()
        .copied()
        .find(|name| {
            symbols
                .lines()
                .any(|line| line.split_whitespace().nth(2) == Some(*name))
        })
        .ok_or_else(|| {
            format!(
                "this kernel has none of {}, the function reads from swap go through",
                SWAP_READ.join(", ")
            )
        })
}

/// Opens a counting kprobe on `symbol` for one thread and what it starts.
fn open_counter(symbol: &str, tid: i32) -> std::io::Result<OwnedFd> {
    let kind = crate::bpf::attach::kprobe_type().map_err(std::io::Error::other)?;
    let name = std::ffi::CString::new(symbol).map_err(std::io::Error::other)?;
    let mut attr = sys::bindings::perf_event_attr {
        type_: kind,
        size: std::mem::size_of::<sys::bindings::perf_event_attr>() as u32,
        ..Default::default()
    };
    attr.__bindgen_anon_3.kprobe_func = name.as_ptr() as u64;
    attr.set_inherit(1);
    attr.set_disabled(1);
    // SAFETY: `attr` is a complete attribute and `name` outlives the call.
    let fd = unsafe {
        sys::perf_event_open(
            &mut attr,
            tid,
            -1,
            -1,
            sys::bindings::PERF_FLAG_FD_CLOEXEC as _,
        )
    };
    drop(name);
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a descriptor the kernel just handed us, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn read_counter(fd: &OwnedFd) -> std::io::Result<u64> {
    let mut value = 0u64;
    // SAFETY: eight bytes into a u64, the read format of a lone counter.
    let read = unsafe {
        libc::read(
            fd.as_raw_fd(),
            (&mut value as *mut u64).cast(),
            std::mem::size_of::<u64>(),
        )
    };
    if read != std::mem::size_of::<u64>() as isize {
        return Err(std::io::Error::last_os_error());
    }
    Ok(value)
}

pub struct SwapIn {
    id: CollectorId,
    mode: Mode,
    symbol: &'static str,
    tids: Vec<i32>,
    key: Option<KeyValue>,
    counters: Vec<OwnedFd>,
    page_size: u64,
    previous: Option<(u64, u64)>,
    opened: Option<(u64, u64)>,
    losses: Losses,
}

impl SwapIn {
    fn total(&mut self) -> (u64, u64) {
        let mut total = 0;
        for fd in &self.counters {
            match read_counter(fd) {
                Ok(value) => total += value,
                Err(_) => self.losses.undecoded += 1,
            }
        }
        (monotonic_ns(), total)
    }

    fn event(&self, before: (u64, u64), after: (u64, u64), seq: u64, basis: &str) -> Event {
        Event {
            ts_ns: after.0,
            ts_err_ns: 0,
            kind: "mem.swap".to_owned(),
            keys: self.key.iter().cloned().collect(),
            attrs: vec![
                // The source's unit is bytes, and a read is a folio of
                // unknown size: reported as one page each, which is what a
                // small folio is, and said in the backend's degradation.
                Attr::uint(
                    "in_bytes",
                    after.1.saturating_sub(before.1) * self.page_size,
                ),
                Attr::uint("swapins", after.1.saturating_sub(before.1)),
                Attr::uint("interval_ns", after.0.saturating_sub(before.0)),
                Attr::text("read_from", self.symbol),
                Attr::text("basis", basis),
            ],
            collector_id: self.id.clone(),
            seq,
        }
    }
}

impl Collector for SwapIn {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "in_bytes"
    }

    fn start(&mut self) -> Result<(), String> {
        for &tid in &self.tids {
            match open_counter(self.symbol, tid) {
                Ok(fd) => self.counters.push(fd),
                // A thread that exited between the listing and the open.
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {}
                Err(error) => {
                    return Err(format!(
                        "the kernel refused a counting kprobe on `{}` for thread {tid} \
                         ({error}); a kprobe needs CAP_PERFMON whatever \
                         kernel.perf_event_paranoid says",
                        self.symbol
                    ))
                }
            }
        }
        if self.counters.is_empty() {
            return Err("every thread of the target exited before it could be followed".into());
        }
        for fd in &self.counters {
            // SAFETY: an ioctl on our own perf descriptor.
            if unsafe { sys::ioctls::ENABLE(fd.as_raw_fd(), 0) } < 0 {
                return Err(format!(
                    "cannot enable the swap-in counter: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        // Every counter starts at zero when it is enabled, so the window
        // opens here and nothing between this and the first tick is lost.
        let opened = (monotonic_ns(), 0);
        self.opened = Some(opened);
        // A poll's first read is its baseline, as the other backends' is: the
        // run reads every collector right after start, and a point covering
        // that fraction of a millisecond would be a point of nothing.
        if self.mode == Mode::Count {
            self.previous = Some(opened);
        }
        Ok(())
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        let now = self.total();
        if self.mode == Mode::Count {
            self.previous = Some(now);
            return Ok(Vec::new());
        }
        let before = self.previous.replace(now);
        Ok(before
            .map(|before| self.event(before, now, seq, "swap reads over this tick"))
            .into_iter()
            .collect())
    }

    /// The counters outlive the threads they follow, inherited counts folded
    /// in at each child's exit, so the last read is taken here.
    fn stop(&mut self) -> Vec<Event> {
        let now = self.total();
        let opened = self.opened.unwrap_or((now.0, 0));
        self.counters.clear();
        if self.mode != Mode::Count {
            return Vec::new();
        }
        vec![self.event(opened, now, u64::MAX / 2, "swap reads over the window")]
    }

    fn losses(&self) -> Losses {
        self.losses.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_kernel_names_its_swap_read() {
        match swap_read_symbol() {
            Ok(symbol) => assert!(SWAP_READ.contains(&symbol)),
            Err(reason) => eprintln!("skipped: {reason}"),
        }
    }

    /// A walker whose working set does not fit under its cgroup's limit reads
    /// its own pages back from swap on every walk. It is the only process in
    /// the cgroup, so the cgroup's `pswpin` -- the kernel's own count, in
    /// pages, and these are small pages -- is the same swap-ins counted
    /// another way.
    #[test]
    fn a_thrashing_walker_s_swap_ins_are_its_cgroup_s() {
        use super::super::cgroup::{stat_field, stat_fields};
        use crate::collect::mem::testing::Scratch;
        const MIB: u64 = 1 << 20;
        if swap_read_symbol().is_err() {
            eprintln!("skipped: no swap read function to probe");
            return;
        }
        let Some(scratch) = Scratch::cgroup("swapin") else {
            eprintln!("skipped: no delegated cgroup with the memory controller");
            return;
        };
        if let Some(reason) = scratch.no_swap_room(96 * MIB) {
            eprintln!("skipped: {reason}");
            return;
        }
        if !scratch.write("memory.high", &(32 * MIB).to_string()) {
            eprintln!("skipped: cannot limit the scratch cgroup");
            return;
        }
        let pswpin = || {
            let text = std::fs::read_to_string(scratch.path().join("memory.stat")).ok()?;
            stat_field(&stat_fields(&text), "pswpin")
        };
        let mut walker = scratch.spawn_walker(96 * MIB, 64 * MIB);
        let mut collector = match Box::new(Prepare::new(Mode::Count).unwrap())
            .attach(&Target::Pid(vec![walker.pid()]))
            .and_then(|mut collector| collector.start().map(|()| collector))
        {
            Ok(collector) => collector,
            Err(reason) => {
                walker.stop();
                eprintln!("skipped: {reason}");
                return;
            }
        };
        let Some(before) = pswpin() else {
            walker.stop();
            eprintln!("skipped: this kernel counts no pswpin per cgroup");
            return;
        };
        walker.wait_for_walks(3);
        let after = pswpin().expect("pswpin");
        let events = collector.stop();
        walker.stop();
        let counted = events[0].attr("swapins").and_then(|v| v.as_f64()).unwrap();
        let pages = (after - before) as f64;
        eprintln!("swap-ins counted {counted}, cgroup pswpin {pages}");
        assert!(
            pages > 1000.0,
            "the walker did not thrash: {pages} pages in"
        );
        assert!(
            (counted - pages).abs() <= 0.05 * pages,
            "the counter read {counted} swap-ins, the cgroup {pages} pages"
        );
    }

    #[test]
    fn a_machine_target_is_sent_to_the_machine_backend() {
        let refusal = Box::new(Prepare::new(Mode::Count).unwrap())
            .attach(&Target::System)
            .err()
            .expect("the machine is not a process");
        assert!(refusal.contains("--system"), "{refusal}");
    }
}
