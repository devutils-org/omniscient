//! Names from Linux perf jitdump, discovered through the runtime's mapped marker.
//! Format: tools/perf/Documentation/jitdump-specification.txt in Linux.
//! Read complete records only; skip generated code, line tables and unwind data.
use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub(crate) struct Code {
    pub address: u64,
    pub size: u64,
    pub symbol: String,
}

#[derive(Debug)]
pub(crate) struct Dump {
    pub path: PathBuf,
    consumed: u64,
    identity: Option<(u64, u64)>,
    little: bool,
    codes: BTreeMap<u64, Code>,
    pub records: u64,
    pub issue: Option<String>,
}

impl Dump {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            consumed: 0,
            identity: None,
            little: true,
            codes: BTreeMap::new(),
            records: 0,
            issue: None,
        }
    }

    fn u32(&self, bytes: &[u8]) -> u32 {
        let bytes = bytes[..4].try_into().unwrap();
        if self.little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        }
    }
    fn u64(&self, bytes: &[u8]) -> u64 {
        let bytes = bytes[..8].try_into().unwrap();
        if self.little {
            u64::from_le_bytes(bytes)
        } else {
            u64::from_be_bytes(bytes)
        }
    }

    pub fn lookup(&self, address: u64) -> Option<&Code> {
        self.codes.values().find(|code| {
            address == code.address
                || (code.address..code.address.saturating_add(code.size)).contains(&address)
        })
    }

    pub fn update(&mut self) -> bool {
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return false;
        };
        let Ok(meta) = file.metadata() else {
            return false;
        };
        let identity = (meta.dev(), meta.ino());
        if self.identity.is_some_and(|old| old != identity) || meta.len() < self.consumed {
            *self = Self::new(self.path.clone());
        }
        self.identity = Some(identity);
        if self.issue.is_some() {
            return false;
        }
        let result = self.read(&mut file, meta.len());
        match result {
            Ok(changed) => changed,
            Err(error) => {
                self.issue = Some(error);
                false
            }
        }
    }

    fn read(&mut self, file: &mut std::fs::File, length: u64) -> Result<bool, String> {
        let io = |error: std::io::Error| error.to_string();
        if self.consumed == 0 {
            if length < 40 {
                return Ok(false);
            }
            file.seek(SeekFrom::Start(0)).map_err(io)?;
            let mut header = [0; 40];
            file.read_exact(&mut header).map_err(io)?;
            self.little = match &header[..4] {
                b"DTiJ" => true,
                b"JiTD" => false,
                _ => return Err("invalid jitdump magic".into()),
            };
            let size = u64::from(self.u32(&header[8..]));
            if self.u32(&header[4..]) != 1 || size < 40 {
                return Err("unsupported jitdump version or header size".into());
            }
            if size > length {
                return Ok(false);
            }
            self.consumed = size;
        }
        let mut changed = false;
        while length.saturating_sub(self.consumed) >= 16 {
            file.seek(SeekFrom::Start(self.consumed)).map_err(io)?;
            let mut header = [0; 16];
            file.read_exact(&mut header).map_err(io)?;
            let kind = self.u32(&header);
            let size = u64::from(self.u32(&header[4..]));
            if size < 16 {
                return Err("jitdump record smaller than its header".into());
            }
            if size > length - self.consumed {
                break;
            } // Runtime is still writing it.
            match kind {
                0 => {
                    if size < 57 {
                        return Err("short JIT_CODE_LOAD".into());
                    }
                    let mut fixed = [0; 40];
                    file.read_exact(&mut fixed).map_err(io)?;
                    let address = self.u64(&fixed[16..]);
                    let code_size = self.u64(&fixed[24..]);
                    let index = self.u64(&fixed[32..]);
                    let name_size = size
                        .checked_sub(56)
                        .and_then(|v| v.checked_sub(code_size))
                        .ok_or("JIT_CODE_LOAD code exceeds record")?;
                    if name_size == 0 || name_size > 1024 * 1024 {
                        return Err("invalid jitdump symbol length".into());
                    }
                    let mut name = vec![0; name_size as usize];
                    file.read_exact(&mut name).map_err(io)?;
                    let end = name
                        .iter()
                        .position(|b| *b == 0)
                        .ok_or("unterminated jitdump symbol")?;
                    let symbol = String::from_utf8_lossy(&name[..end]).into_owned();
                    // A reused range supersedes the previous code, like a perf map.
                    self.codes
                        .retain(|_, old| !overlaps(old.address, old.size, address, code_size));
                    self.codes.insert(
                        index,
                        Code {
                            address,
                            size: code_size,
                            symbol,
                        },
                    );
                    changed = true;
                }
                1 => {
                    if size < 64 {
                        return Err("short JIT_CODE_MOVE".into());
                    }
                    let mut fixed = [0; 48];
                    file.read_exact(&mut fixed).map_err(io)?;
                    let old = self.u64(&fixed[16..]);
                    let address = self.u64(&fixed[24..]);
                    let code_size = self.u64(&fixed[32..]);
                    let index = self.u64(&fixed[40..]);
                    let code = self
                        .codes
                        .get(&index)
                        .filter(|c| c.address == old && c.size == code_size)
                        .cloned()
                        .ok_or("JIT_CODE_MOVE has no matching load")?;
                    self.codes.remove(&index);
                    self.codes
                        .retain(|_, old| !overlaps(old.address, old.size, address, code_size));
                    self.codes.insert(index, Code { address, ..code });
                    changed = true;
                }
                _ => {} // Debug info, close, unwinding info and future records.
            }
            self.records += 1;
            self.consumed += size;
        }
        Ok(changed)
    }
}

fn overlaps(a: u64, a_size: u64, b: u64, b_size: u64) -> bool {
    a < b.saturating_add(b_size.max(1)) && b < a.saturating_add(a_size.max(1))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    fn u32_bytes(value: u32, big: bool) -> [u8; 4] {
        if big {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }
    fn u64_bytes(value: u64, big: bool) -> [u8; 8] {
        if big {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        }
    }
    pub(crate) fn fixture(big: bool) -> Vec<u8> {
        let mut data = Vec::new();
        for value in [0x4a695444, 1, 40, 62, 0, std::process::id()] {
            data.extend(u32_bytes(value, big));
        }
        data.extend(u64_bytes(0, big));
        data.extend(u64_bytes(0, big));
        data.extend(u32_bytes(0, big));
        data.extend(u32_bytes(65, big));
        data.extend(u64_bytes(1, big)); // Record timestamp.
        data.extend(u32_bytes(std::process::id(), big));
        data.extend(u32_bytes(std::process::id(), big));
        for value in [0x1000, 0x1000, 4, 1] {
            data.extend(u64_bytes(value, big));
        }
        data.extend(b"work\0");
        data.extend([0x90; 4]);
        data
    }
    fn path() -> PathBuf {
        std::env::temp_dir().join(format!(
            "omn-jitdump-{}-{}.dump",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn complete_loads_are_read_in_both_byte_orders_and_partial_records_wait() {
        for big in [false, true] {
            let path = path();
            let bytes = fixture(big);
            std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
            let mut dump = Dump::new(path.clone());
            assert!(!dump.update());
            assert!(dump.lookup(0x1001).is_none());
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(&bytes[bytes.len() - 1..])
                .unwrap();
            assert!(dump.update());
            assert!(!dump.update());
            assert_eq!(dump.lookup(0x1001).unwrap().symbol, "work");
            assert!(dump.lookup(0x1004).is_none());
            assert_eq!(dump.records, 1);
            assert!(dump.issue.is_none());
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn moves_keep_the_name_and_remove_the_old_address() {
        for big in [false, true] {
            let path = path();
            let mut bytes = fixture(big);
            bytes.extend(u32_bytes(1, big));
            bytes.extend(u32_bytes(64, big));
            bytes.extend(u64_bytes(2, big));
            bytes.extend(u32_bytes(std::process::id(), big));
            bytes.extend(u32_bytes(std::process::id(), big));
            for value in [0x2000, 0x1000, 0x2000, 4, 1] {
                bytes.extend(u64_bytes(value, big));
            }
            std::fs::write(&path, bytes).unwrap();
            let mut dump = Dump::new(path.clone());
            assert!(dump.update());
            assert!(dump.lookup(0x1000).is_none());
            assert_eq!(dump.lookup(0x2002).unwrap().symbol, "work");
            assert_eq!(dump.records, 2);
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn corrupt_records_are_reported_and_do_not_loop_or_allocate_the_declared_size() {
        let path = path();
        let mut bytes = fixture(false);
        bytes.extend(0u32.to_le_bytes());
        bytes.extend(1u32.to_le_bytes());
        bytes.extend(0u64.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        let mut dump = Dump::new(path.clone());
        dump.update();
        assert!(dump.issue.as_ref().unwrap().contains("smaller"));
        assert!(!dump.update());
        assert_eq!(dump.lookup(0x1000).unwrap().symbol, "work");
        std::fs::remove_file(path).unwrap();
    }
}
