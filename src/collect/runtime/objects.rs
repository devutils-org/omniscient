//! Which ELF objects a target maps, and what USDT probes each one carries.
//!
//! A probe lives in a file, not in a process: `libc:setjmp` is in
//! `libc.so.6`, and a uprobe on that file fires in every process that maps it.
//! So before anything can be attached the question is which files the target
//! has, and it has two answers depending on whether the target exists yet.
//!
//! **A running process** says so itself: `/proc/<pid>/maps` lists every file it
//! has mapped, including libraries it opened with `dlopen`.
//!
//! **A launched program** is attached before it execs -- that is the point of
//! the launch gate -- so at that moment its map is still this tool's own. What
//! it *will* map is read the way the dynamic linker would decide it: the
//! executable, found on `PATH`; its interpreter; and the closure of its
//! `DT_NEEDED` entries, searched through its own `DT_RUNPATH`, then
//! `LD_LIBRARY_PATH`, then the directories `/etc/ld.so.conf` names, then the
//! default ones. That is an approximation of `ld.so`, and it misses exactly one
//! thing on purpose: a library the program will `dlopen` later is not named
//! anywhere before it runs. The run says which objects it read, so a probe in
//! a library it did not read is a visible absence rather than a silent one.

use std::collections::{BTreeSet, VecDeque};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::bpf::usdt::{self, Object, Site};

/// What one object contributed.
pub struct Scanned {
    /// Where this tool opened it: for a running process,
    /// `/proc/<pid>/root/...` or another way to its mapped inode
    /// ([`crate::stack::reach_mapped`]), which is also what a uprobe is
    /// attached through.
    pub path: PathBuf,
    /// The name the target knows it by, which is what is shown.
    pub mapped: PathBuf,
    /// Its probe sites, or why it could not be read.
    pub sites: Result<Vec<Site>, String>,
    /// What the ELF actually contained, in words.
    pub description: String,
}

/// Every object considered, and the names that could not be found.
pub struct Objects {
    pub scanned: Vec<Scanned>,
    /// `DT_NEEDED` names no search path resolved.
    pub unresolved: Vec<String>,
}

impl Objects {
    /// Every site in every object that could be read, in a stable order.
    pub fn sites(&self) -> Vec<Site> {
        self.scanned
            .iter()
            .filter_map(|scanned| scanned.sites.as_ref().ok())
            .flatten()
            .cloned()
            .collect()
    }

    /// The whole inventory in one paragraph, which is what every refusal in
    /// this family ends with: not "no probe", but what was looked at and what
    /// each file actually held.
    pub fn inventory(&self) -> String {
        let mut lines: Vec<String> = self
            .scanned
            .iter()
            .map(|scanned| format!("  {}", scanned.description))
            .collect();
        if !self.unresolved.is_empty() {
            lines.push(format!(
                "  and {} needed librar{} no search path resolved: {}",
                self.unresolved.len(),
                if self.unresolved.len() == 1 {
                    "y"
                } else {
                    "ies"
                },
                self.unresolved.join(", ")
            ));
        }
        lines.join("\n")
    }
}

fn scan(paths: Vec<PathBuf>, unresolved: Vec<String>) -> Objects {
    scan_reached(
        paths.into_iter().map(|path| (path.clone(), path, None)),
        unresolved,
    )
}

/// Each object read at its first path and shown by its second; the third
/// says why a file outside this tool's reach could not be read.
fn scan_reached(
    paths: impl Iterator<Item = (PathBuf, PathBuf, Option<String>)>,
    unresolved: Vec<String>,
) -> Objects {
    let scanned = paths
        .map(|(path, mapped, unreachable)| {
            // Shown by the name the target maps, not the `/proc` path read.
            let shown = |text: String| {
                if path == mapped {
                    text
                } else {
                    text.replace(&path.display().to_string(), &mapped.display().to_string())
                }
            };
            match Object::read(&path) {
                Ok(object) => Scanned {
                    description: shown(object.describe()),
                    sites: usdt::sites(&object),
                    path,
                    mapped,
                },
                Err(error) => {
                    let error = match &unreachable {
                        Some(why) => format!("{}; {why}", shown(error)),
                        None => shown(error),
                    };
                    Scanned {
                        description: error.clone(),
                        sites: Err(error),
                        path,
                        mapped,
                    }
                }
            }
        })
        .collect();
    Objects {
        scanned,
        unresolved,
    }
}

/// The objects a running process has mapped, each opened through the
/// target's own mount namespace and checked to be the inode it maps
/// ([`crate::stack::reach_mapped`]): a process in a container names files
/// this tool's namespace does not have, or has as other files.
pub fn of_process(pid: i32) -> Result<Objects, String> {
    let maps = fs::read_to_string(format!("/proc/{pid}/maps"))
        .map_err(|error| format!("cannot read the memory map of process {pid}: {error}"))?;
    let mappings = usdt::parse_maps(&maps);
    let mut seen = BTreeSet::new();
    let paths: Vec<PathBuf> = mappings
        .iter()
        .map(|mapping| mapping.path.clone())
        // A file replaced on disk since it was mapped is shown with a suffix,
        // and the file at that path now is not the one in the process.
        .filter(|path| !path.to_string_lossy().ends_with(" (deleted)"))
        .filter(|path| seen.insert(path.clone()))
        .collect();
    let reached = paths.into_iter().map(|path| {
        let identities: Vec<crate::stack::MappedIdentity> = mappings
            .iter()
            .filter(|mapping| mapping.path == path)
            .map(|mapping| crate::stack::MappedIdentity {
                device: mapping.device,
                inode: mapping.inode,
                start: mapping.start,
                end: mapping.end,
            })
            .collect();
        match crate::stack::reach_mapped(pid, &path, &identities) {
            crate::stack::Reached::MapFiles(through) => (
                through,
                path,
                Some(format!(
                    "neither /proc/{pid}/root nor this namespace has the inode process {pid} \
                     maps at that path (replaced since, or out of reach), and its mapping \
                     under /proc/{pid}/map_files is readable only with CAP_SYS_ADMIN"
                )),
            ),
            other => (other.path().to_owned(), path, None),
        }
    });
    Ok(scan_reached(reached, Vec::new()))
}

/// The objects a program about to be launched will map, as far as can be told
/// before it runs.
pub fn of_launch(argv: &[String]) -> Result<Objects, String> {
    let program = argv
        .first()
        .ok_or_else(|| "a launch target names no program".to_owned())?;
    let executable = find_program(program).ok_or_else(|| {
        format!("`{program}` is not a file and is not on PATH, so there is no object to read")
    })?;
    let executable = interpreter_of_script(&executable).unwrap_or(executable);

    let mut paths = Vec::new();
    let mut unresolved = Vec::new();
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([executable]);
    let system = system_directories();
    while let Some(path) = queue.pop_front() {
        let canonical = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen.insert(canonical.clone()) {
            continue;
        }
        paths.push(canonical.clone());
        let Ok(object) = Object::read(&canonical) else {
            continue;
        };
        let dynamic = object.dynamic();
        if let Some(interpreter) = dynamic.interpreter {
            queue.push_back(interpreter);
        }
        let origin = canonical
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let own: Vec<PathBuf> = dynamic
            .search
            .iter()
            .map(|entry| PathBuf::from(entry.replace("$ORIGIN", &origin.to_string_lossy())))
            .collect();
        for name in dynamic.needed {
            match resolve_library(&name, &own, &system) {
                Some(found) => queue.push_back(found),
                None => unresolved.push(name),
            }
        }
    }
    unresolved.sort();
    unresolved.dedup();
    Ok(scan(paths, unresolved))
}

/// `argv[0]` the way `execvp` finds it.
fn find_program(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return path.is_file().then_some(path);
    }
    env::var_os("PATH").and_then(|path| {
        env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// A script's interpreter, when the program is a `#!` script: the probes are
/// in the interpreter, not in the text.
fn interpreter_of_script(path: &Path) -> Option<PathBuf> {
    // The first line is all that matters, and the file may be a binary of
    // tens of megabytes.
    let mut head = [0u8; 256];
    let read = {
        use std::io::Read;
        fs::File::open(path).ok()?.read(&mut head).ok()?
    };
    let line = head[..read].strip_prefix(b"#!")?;
    let line = &line[..line
        .iter()
        .position(|byte| *byte == b'\n')
        .unwrap_or(line.len())];
    let line = String::from_utf8_lossy(line);
    let mut words = line.split_whitespace();
    let interpreter = words.next()?;
    // `#!/usr/bin/env python3` names its interpreter in the second word.
    if interpreter.ends_with("/env") {
        return words
            .find(|word| !word.starts_with('-'))
            .and_then(find_program);
    }
    Some(PathBuf::from(interpreter))
}

fn resolve_library(name: &str, own: &[PathBuf], system: &[PathBuf]) -> Option<PathBuf> {
    if name.contains('/') {
        let path = PathBuf::from(name);
        return path.is_file().then_some(path);
    }
    let from_environment: Vec<PathBuf> = env::var_os("LD_LIBRARY_PATH")
        .map(|value| env::split_paths(&value).collect())
        .unwrap_or_default();
    own.iter()
        .chain(&from_environment)
        .chain(system)
        .map(|dir| dir.join(name))
        .find(|candidate| is_this_machines_elf(candidate))
}

/// A library of the right name and the wrong architecture is skipped, the way
/// the linker skips it: a 32-bit `libc.so.6` in `/usr/lib32` is not the one a
/// 64-bit program maps.
fn is_this_machines_elf(path: &Path) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut header = [0u8; 20];
    use std::io::Read;
    if (&file).read_exact(&mut header).is_err() {
        return false;
    }
    // EM_X86_64 and EM_AARCH64: the machine this build runs on, the one whose
    // libraries a program here maps.
    let machine = if cfg!(target_arch = "aarch64") {
        183
    } else {
        62
    };
    &header[..4] == b"\x7fELF"
        && header[4] == 2
        && u16::from_le_bytes([header[18], header[19]]) == machine
}

/// The directories `/etc/ld.so.conf` names, then the linker's defaults.
fn system_directories() -> Vec<PathBuf> {
    let mut directories = Vec::new();
    read_ld_so_conf(Path::new("/etc/ld.so.conf"), &mut directories, 0);
    for default in [
        "/lib64",
        "/usr/lib64",
        // Debian's multiarch directories, for whichever of the two this is.
        if cfg!(target_arch = "aarch64") {
            "/lib/aarch64-linux-gnu"
        } else {
            "/lib/x86_64-linux-gnu"
        },
        if cfg!(target_arch = "aarch64") {
            "/usr/lib/aarch64-linux-gnu"
        } else {
            "/usr/lib/x86_64-linux-gnu"
        },
        "/lib",
        "/usr/lib",
    ] {
        directories.push(PathBuf::from(default));
    }
    directories
}

fn read_ld_so_conf(path: &Path, into: &mut Vec<PathBuf>, depth: usize) {
    if depth > 4 {
        return;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if let Some(pattern) = line.strip_prefix("include") {
            for included in expand_include(pattern.trim()) {
                read_ld_so_conf(&included, into, depth + 1);
            }
        } else if line.starts_with('/') {
            into.push(PathBuf::from(line));
        }
    }
}

/// `include ld.so.conf.d/*.conf`: a directory and a suffix, which is the only
/// shape distributions write.
fn expand_include(pattern: &str) -> Vec<PathBuf> {
    let pattern = if pattern.starts_with('/') {
        PathBuf::from(pattern)
    } else {
        Path::new("/etc").join(pattern)
    };
    let (Some(dir), Some(file)) = (pattern.parent(), pattern.file_name()) else {
        return Vec::new();
    };
    let file = file.to_string_lossy();
    let Some(suffix) = file.strip_prefix('*') else {
        return vec![pattern.clone()];
    };
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.to_string_lossy().ends_with(suffix))
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every dynamically linked program on this machine needs libc, and the
    /// closure has to find the same one the linker maps into a running
    /// process -- which this test process is.
    #[test]
    fn a_launch_finds_the_libc_a_running_process_actually_maps() {
        let Ok(launched) = of_launch(&["sh".to_owned()]) else {
            eprintln!("skipped: no `sh` on PATH");
            return;
        };
        let running = of_process(std::process::id() as i32).expect("this process's own map");
        let libc_of = |objects: &Objects| {
            objects
                .scanned
                .iter()
                .map(|scanned| scanned.path.clone())
                .find(|path| path.file_name().is_some_and(|name| name == "libc.so.6"))
        };
        let (Some(predicted), Some(mapped)) = (libc_of(&launched), libc_of(&running)) else {
            eprintln!(
                "skipped: no libc.so.6 in one of the two views; launch read:\n{}",
                launched.inventory()
            );
            return;
        };
        assert_eq!(
            fs::canonicalize(predicted).unwrap(),
            fs::canonicalize(mapped).unwrap()
        );
        assert!(launched.unresolved.is_empty(), "{}", launched.inventory());
    }

    #[test]
    fn running_process_objects_are_opened_through_its_root() {
        let pid = std::process::id() as i32;
        let objects = of_process(pid).unwrap();
        let root = PathBuf::from(format!("/proc/{pid}/root"));
        assert!(!objects.scanned.is_empty());
        assert!(objects
            .scanned
            .iter()
            .all(|object| object.path.starts_with(&root)));
        let executable = fs::read_link("/proc/self/exe").unwrap();
        let object = objects
            .scanned
            .iter()
            .find(|object| object.path == crate::stack::target_path(pid, &executable))
            .unwrap();
        assert!(object.sites.is_ok(), "{}", object.description);
        assert!(!crate::stack::functions_in(&object.path, &|_| true).is_empty());
    }

    #[test]
    fn process_in_a_distinct_root_has_discoverable_and_symbolizable_functions() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::{Command, Stdio};

        const MARKER: &str = "OMN_TEST_CHROOT_CHILD";
        if std::env::var_os(MARKER).is_none() {
            let output = Command::new("unshare")
                .args(["--user", "--map-root-user", "--mount"])
                .arg(std::env::current_exe().unwrap())
                .args(["--exact", "collect::runtime::objects::tests::process_in_a_distinct_root_has_discoverable_and_symbolizable_functions", "--nocapture"])
                .env(MARKER, "1")
                .output();
            match output {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    eprintln!("skipped: unshare unavailable");
                    return;
                }
                Err(error) => panic!("unshare: {error}"),
                Ok(output)
                    if !output.status.success()
                        && String::from_utf8_lossy(&output.stderr)
                            .contains("Operation not permitted") =>
                {
                    eprintln!("skipped: user namespaces unavailable");
                    return;
                }
                Ok(output) => assert!(
                    output.status.success(),
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ),
            }
            return;
        }
        let dir = env::temp_dir().join(format!("omn-foreign-root-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let source = dir.join("fixture.c");
        fs::write(&source, "#include <stdio.h>\n__attribute__((noinline)) void omn_foreign_marker(void) {}\nint main(void) { printf(\"%p\\n\", (void*)omn_foreign_marker); fflush(stdout); getchar(); return 0; }\n").unwrap();
        let compiled = Command::new("cc")
            .args(["-static", "-g", "-O0", "-o"])
            .arg(dir.join("fixture"))
            .arg(&source)
            .output()
            .expect("C compiler");
        if !compiled.status.success() {
            eprintln!(
                "skipped: static C fixture unavailable: {}",
                String::from_utf8_lossy(&compiled.stderr)
            );
            return;
        }
        assert!(!Path::new("/fixture").exists());
        let mut child = Command::new("chroot")
            .arg(&dir)
            .arg("/fixture")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("chroot fixture");
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let addr =
            u64::from_str_radix(line.trim().trim_start_matches("0x"), 16).expect("fixture address");
        let pid = child.id() as i32;
        let objects = of_process(pid).unwrap();
        let fixture = objects
            .scanned
            .iter()
            .find(|item| item.path.file_name().is_some_and(|name| name == "fixture"))
            .unwrap_or_else(|| {
                panic!(
                    "fixture in target maps: {}",
                    fs::read_to_string(format!("/proc/{pid}/maps")).unwrap_or_default()
                )
            });
        assert!(
            !crate::stack::functions_in(&fixture.path, &|symbol| symbol.symbol
                == "omn_foreign_marker")
            .is_empty()
        );
        let mut symbolizer = crate::stack::Symbolizer::for_process(pid);
        let stack = symbolizer.symbolize(&crate::stack::RawStack {
            user: vec![addr],
            ..Default::default()
        });
        assert_eq!(
            stack.frames[0].symbol.as_deref(),
            Some("omn_foreign_marker")
        );
        child.stdin.take().unwrap().write_all(b"\n").unwrap();
        assert!(child.wait().unwrap().success());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_script_is_read_through_its_interpreter() {
        let dir = env::temp_dir().join(format!("omn-objects-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("script");
        fs::write(&script, "#!/usr/bin/env sh\necho\n").unwrap();
        let interpreter = interpreter_of_script(&script);
        fs::remove_dir_all(&dir).ok();
        let Some(interpreter) = interpreter else {
            eprintln!("skipped: no `sh` on PATH");
            return;
        };
        assert!(interpreter.ends_with("sh"), "{}", interpreter.display());
    }
}
