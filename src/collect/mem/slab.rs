//! `mem.slab` over `/proc/slabinfo`: the kernel's slab caches, one event per
//! cache per poll, what `slabtop` prints.
//!
//! `mem.system` carries the machine's slab total out of `/proc/meminfo`; this
//! is the split by cache, which is the file only root can read. On a machine
//! that does not grant it the backend is missing with that reason, never
//! served by the total.
//!
//! The value is the cache's size the way `slabtop`'s CACHE SIZE column computes
//! it: its slabs times the pages each slab takes, in bytes -- what the cache
//! holds from the page allocator, free objects included. `active_bytes` is
//! what its live objects take, and `use` the share of objects in use, which is
//! the column a leak shows in first.
//!
//! The format is version 2.x, unchanged since 2.6.13; a header that names
//! another version is refused rather than read by position.

use std::path::PathBuf;
use std::time::Duration;

use super::cadence::Cadence;
use crate::clock::monotonic_ns;
use crate::collect::{Collector, Prepared, SampleError};
use crate::event::{Attr, CollectorId, Event};
use crate::request::{Mode, Target};

const SLABINFO: &str = "/proc/slabinfo";

/// One cache's line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cache {
    name: String,
    active_objs: u64,
    num_objs: u64,
    objsize: u64,
    pages_per_slab: u64,
    num_slabs: u64,
}

fn page_size() -> u64 {
    // SAFETY: `sysconf` takes an integer and returns one; no memory is touched.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size > 0 {
        size as u64
    } else {
        4096
    }
}

/// The caches of a `/proc/slabinfo`, or why it cannot be read.
fn parse(text: &str) -> Result<Vec<Cache>, String> {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    let version = header
        .strip_prefix("slabinfo - version: ")
        .ok_or_else(|| format!("{SLABINFO} does not open with its version line: `{header}`"))?;
    if !version.trim().starts_with("2.") {
        return Err(format!(
            "{SLABINFO} is version {version}, and this reader knows the columns of 2.x"
        ));
    }
    let mut caches = Vec::new();
    for line in lines.filter(|line| !line.starts_with('#')) {
        // `name active_objs num_objs objsize objperslab pagesperslab : tunables
        // l b s : slabdata active_slabs num_slabs sharedavail`
        let words: Vec<&str> = line.split_whitespace().collect();
        let number = |at: usize| words.get(at).and_then(|word| word.parse::<u64>().ok());
        let slabdata = words.iter().position(|word| *word == "slabdata");
        let (Some(name), Some(active_objs), Some(num_objs), Some(objsize), Some(pages_per_slab)) =
            (words.first(), number(1), number(2), number(3), number(5))
        else {
            continue;
        };
        let Some(num_slabs) = slabdata.and_then(|at| number(at + 2)) else {
            continue;
        };
        caches.push(Cache {
            name: (*name).to_owned(),
            active_objs,
            num_objs,
            objsize,
            pages_per_slab,
            num_slabs,
        });
    }
    Ok(caches)
}

fn attributes(cache: &Cache, page: u64) -> Vec<Attr> {
    let size = cache
        .num_slabs
        .saturating_mul(cache.pages_per_slab)
        .saturating_mul(page);
    let mut attrs = vec![
        Attr::text("cache", cache.name.clone()),
        Attr::uint("bytes", size),
        Attr::uint(
            "active_bytes",
            cache.active_objs.saturating_mul(cache.objsize),
        ),
        Attr::uint("active_objs", cache.active_objs),
        Attr::uint("objs", cache.num_objs),
        Attr::uint("objsize", cache.objsize),
        Attr::uint("slabs", cache.num_slabs),
        Attr::uint("pages_per_slab", cache.pages_per_slab),
    ];
    if cache.num_objs > 0 {
        attrs.push(Attr::float(
            "use",
            cache.active_objs as f64 / cache.num_objs as f64,
        ));
    }
    attrs
}

pub struct Prepare(Mode);

impl Prepare {
    pub fn new(mode: Mode) -> Result<Self, String> {
        match mode {
            Mode::Poll { .. } | Mode::Snapshot => Ok(Self(mode)),
            other => Err(format!(
                "{SLABINFO} is a set of levels that is read, not a stream of events: `{}` cannot \
                 be served from it; poll it, or take one snapshot",
                other.kind()
            )),
        }
    }
}

impl Prepared for Prepare {
    fn attach(self: Box<Self>, target: &Target) -> Result<Box<dyn Collector>, String> {
        if !matches!(target, Target::System) {
            return Err(format!(
                "{SLABINFO} is the kernel's, and a {} target is not the machine: slab memory is \
                 charged to a cgroup in its `memory.stat` (`slab`), which is `mem.cgroup`",
                target.kind()
            ));
        }
        let interval = match self.0 {
            Mode::Poll { interval } => Duration::from_nanos(interval.as_nanos()),
            _ => Duration::ZERO,
        };
        Ok(Box::new(SlabInfo {
            id: CollectorId::new("mem.slab/fs:proc"),
            path: PathBuf::from(SLABINFO),
            cadence: Cadence::new(interval),
        }))
    }
}

pub struct SlabInfo {
    id: CollectorId,
    path: PathBuf,
    cadence: Cadence,
}

impl Collector for SlabInfo {
    fn id(&self) -> &CollectorId {
        &self.id
    }

    fn value_attr(&self) -> &'static str {
        "bytes"
    }

    fn sample(&mut self, seq: u64) -> Result<Vec<Event>, SampleError> {
        if !self.cadence.due(monotonic_ns()) {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path)
            .map_err(|error| SampleError::Transient(format!("{SLABINFO}: {error}")))?;
        let ts_ns = monotonic_ns();
        let caches = parse(&text).map_err(SampleError::Transient)?;
        let page = page_size();
        Ok(caches
            .iter()
            .enumerate()
            .map(|(index, cache)| Event {
                ts_ns,
                ts_err_ns: 0,
                kind: "mem.slab".to_owned(),
                keys: Vec::new(),
                attrs: attributes(cache, page),
                collector_id: self.id.clone(),
                seq: seq + index as u64,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three lines of a real 6.x `/proc/slabinfo` (SLUB, `tunables` all zero).
    const SLABINFO_TEXT: &str = "slabinfo - version: 2.1
# name            <active_objs> <num_objs> <objsize> <objperslab> <pagesperslab> : tunables <limit> <batchcount> <sharedfactor> : slabdata <active_slabs> <num_slabs> <sharedavail>
dentry            273714 283710    192   42    2 : tunables    0    0    0 : slabdata   6755   6755      0
kmalloc-64         45632  46720     64   64    1 : tunables    0    0    0 : slabdata    730    730      0
ext4_inode_cache   88051  90363   1176   27    8 : tunables    0    0    0 : slabdata   3347   3347      0
";

    #[test]
    fn a_cache_is_its_slabs_times_their_pages_as_slabtop_sizes_it() {
        let caches = parse(SLABINFO_TEXT).expect("parsed");
        assert_eq!(caches.len(), 3);
        let dentry = attributes(&caches[0], 4096);
        let get = |attrs: &[Attr], name: &str| {
            attrs
                .iter()
                .find(|attr| attr.name == name)
                .map(|attr| attr.value.clone())
        };
        // slabtop: 6755 slabs x 2 pages x 4 KiB = 55,336,960 B = 54040K.
        assert_eq!(
            get(&dentry, "bytes"),
            Some(crate::event::AttrValue::Uint(6755 * 2 * 4096))
        );
        assert_eq!(
            get(&dentry, "active_bytes"),
            Some(crate::event::AttrValue::Uint(273_714 * 192))
        );
        assert_eq!(
            get(&dentry, "cache"),
            Some(crate::event::AttrValue::Text("dentry".to_owned()))
        );
        assert_eq!(caches[2].pages_per_slab, 8);
    }

    #[test]
    fn another_version_is_refused_rather_than_read_by_position() {
        let refused = parse("slabinfo - version: 1.1\nfoo 1 2 3\n").unwrap_err();
        assert!(refused.contains("1.1"), "{refused}");
        assert!(parse("garbage").is_err());
    }

    #[test]
    fn every_declared_filter_field_and_key_is_on_an_event() {
        let catalog = crate::catalog::Catalog::embedded().expect("the catalog");
        let entry = catalog
            .get(&"mem.slab".parse().expect("an id"))
            .expect("mem.slab");
        let attrs = attributes(&parse(SLABINFO_TEXT).unwrap()[0], 4096);
        for field in &entry.filter_fields {
            assert!(attrs.iter().any(|attr| attr.name == *field), "{field}");
        }
    }

    #[test]
    fn a_process_target_is_refused_with_where_its_slab_is() {
        let refused = Box::new(Prepare::new(Mode::Snapshot).unwrap())
            .attach(&Target::Pid(vec![1]))
            .err()
            .expect("refused");
        assert!(refused.contains("mem.cgroup"), "{refused}");
    }
}
