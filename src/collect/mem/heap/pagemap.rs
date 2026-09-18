//! Which pages of an address range are actually in memory.
//!
//! `/proc/[pid]/pagemap` has one 64-bit entry per page of the address space.
//! Bit 63 says the page is present, bit 62 that it has been swapped out. The
//! page frame number needs privileges and is zeroed for everyone else, which is
//! fine: this asks whether a page exists, not where it is.
//!
//! This is what turns "the program freed that memory" into "the process still
//! holds it". RSS answers the same question for the whole process; scoped to
//! the ranges an allocator was given and gave back, it answers it for the
//! allocator.

use std::fs::File;
use std::io;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;

const PRESENT: u64 = 1 << 63;
const SWAPPED: u64 = 1 << 62;

/// Entries read in one syscall. 4096 entries cover 16 MiB of address space.
const BATCH: usize = 4096;

/// What a scan found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Residency {
    /// Bytes backed by a page in memory.
    pub resident: u64,
    /// Bytes that were paged out. Not resident, and not gone either: the
    /// process still owns them, and touching them costs a major fault.
    pub swapped: u64,
    /// Pages the scan looked at, so the cost of the scan is visible.
    pub pages_examined: u64,
}

pub struct Pagemap {
    path: PathBuf,
    page_size: u64,
}

impl Pagemap {
    /// Checks that the file can be opened, and keeps the path rather than the
    /// descriptor.
    ///
    /// The descriptor is deliberately not held. The kernel binds an open
    /// `pagemap` to the address space the process had at the time, and a
    /// program the tool launches replaces its address space when it execs: a
    /// descriptor opened before that answers for a memory map that no longer
    /// exists, and it answers with zeroes rather than with an error. One open
    /// per scan costs two syscalls every poll and removes the whole class of
    /// question.
    pub fn open(pid: i32) -> io::Result<Self> {
        let path = PathBuf::from(format!("/proc/{pid}/pagemap"));
        File::open(&path)?;
        Ok(Self {
            path,
            page_size: page_size(),
        })
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Adds up the resident bytes of every page fully inside one of `ranges`.
    ///
    /// Fully inside, deliberately. A page straddling the boundary between a
    /// freed range and a live one holds both, and the kernel cannot say in what
    /// proportion; counting it here would credit fragmentation with memory the
    /// program is still using. So the boundary page goes to the live side and
    /// this number is a lower bound -- at most one page per end of each range,
    /// which for a table of merged ranges is small.
    pub fn scan(&mut self, ranges: impl Iterator<Item = (u64, u64)>) -> io::Result<Residency> {
        let file = File::open(&self.path)?;
        let mut found = Residency::default();
        let mut entries = vec![0u8; BATCH * 8];

        for (base, end) in ranges {
            let first = base.div_ceil(self.page_size);
            let last = end / self.page_size;
            let mut page = first;
            while page < last {
                let wanted = ((last - page) as usize).min(BATCH);
                let bytes = &mut entries[..wanted * 8];
                let read = file.read_at(bytes, page * 8)?;
                if read == 0 {
                    // The kernel answers for a mapped page and for an unmapped
                    // one alike; it stops answering when there is no address
                    // space left to ask about, which for a range the program
                    // allocated means the process is gone. Reporting the pages
                    // that were never read as absent would turn a dead target
                    // into a measurement of zero.
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        format!("{} stopped answering at page {page}", self.path.display()),
                    ));
                }
                for entry in bytes[..read].chunks_exact(8) {
                    let entry = u64::from_ne_bytes(entry.try_into().expect("8 bytes"));
                    if entry & PRESENT != 0 {
                        found.resident += self.page_size;
                    } else if entry & SWAPPED != 0 {
                        found.swapped += self.page_size;
                    }
                }
                let examined = (read / 8) as u64;
                found.pages_examined += examined;
                page += examined;
            }
        }
        Ok(found)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Ground truth against memory this test made resident itself.
    ///
    /// A buffer that has been written to is resident by definition, and one
    /// that has only been reserved is not. If the scan cannot tell those two
    /// apart, it is reading the wrong bits.
    #[test]
    fn a_touched_range_reads_as_resident_and_an_untouched_one_does_not() {
        let page = page_size() as usize;
        let pages = 512;

        let touched = vec![1u8; pages * page];
        let mut pagemap = match Pagemap::open(std::process::id() as i32) {
            Ok(pagemap) => pagemap,
            Err(error) => {
                eprintln!("skipped: this pagemap cannot be read: {error}");
                return;
            }
        };

        let base = touched.as_ptr() as u64;
        let found = pagemap
            .scan([(base, base + (pages * page) as u64)].into_iter())
            .expect("the scan reads our own address space");
        assert!(
            found.resident as f64 >= 0.9 * (pages * page) as f64,
            "a buffer this process wrote to should be resident, got {found:?}"
        );

        // An address range this process never mapped holds nothing. The kernel
        // answers with zeroed entries rather than an error.
        let empty = pagemap
            .scan([(0x7000_0000_0000, 0x7000_0000_0000 + (pages * page) as u64)].into_iter())
            .expect("an unmapped range reads as absent");
        assert_eq!(empty.resident, 0, "an unmapped range cannot be resident");
    }

    #[test]
    fn only_whole_pages_inside_the_range_are_counted() {
        let page = page_size();
        let mut pagemap = match Pagemap::open(std::process::id() as i32) {
            Ok(pagemap) => pagemap,
            Err(_) => return,
        };
        // A range shorter than a page contains no whole page, whatever it
        // overlaps.
        let inside = pagemap
            .scan([(page + 8, page + 16)].into_iter())
            .expect("a scan of nothing");
        assert_eq!(inside.pages_examined, 0);
    }
}
