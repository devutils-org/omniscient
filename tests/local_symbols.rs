//! A stripped DSO's private function is named from a pre-existing debuginfod
//! cache, without a server, and a mismatched cached ELF is never used.
use std::process::Command;

#[test]
fn local_symbols_use_only_matching_cached_debug_info() {
    if let Some(library) = std::env::var_os("OMN_TEST_LOCAL_DSO") {
        let library = std::ffi::CString::new(library.as_encoded_bytes()).unwrap();
        unsafe {
            let handle = libc::dlopen(library.as_ptr(), libc::RTLD_NOW);
            assert!(!handle.is_null());
            let getter = libc::dlsym(handle, c"worker_address".as_ptr());
            assert!(!getter.is_null());
            let getter: unsafe extern "C" fn() -> *const libc::c_void = std::mem::transmute(getter);
            let address = getter() as u64;
            let mut symbols = omn::stack::Symbolizer::for_process(std::process::id() as i32);
            let stack = symbols.symbolize(&omn::stack::RawStack {
                user: vec![address],
                ..Default::default()
            });
            let frame = &stack.frames[0];
            if std::env::var_os("OMN_TEST_MISMATCH").is_some() {
                assert!(frame.symbol.is_none(), "{frame:?}");
            } else {
                assert_eq!(frame.symbol.as_deref(), Some("local_worker"));
                assert_eq!(frame.named_by, Some(omn::stack::NamedBy::Debuginfod));
            }
            libc::dlclose(handle);
        }
        return;
    }
    let dir = std::env::temp_dir().join(format!("omn-local-symbols-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let source = dir.join("worker.c");
    std::fs::write(&source, "static int local_worker(void) { return 7; }\nvoid *worker_address(void) { return local_worker; }\n").unwrap();
    let library = dir.join("worker.so");
    checked(
        Command::new("cc")
            .args(["-shared", "-fPIC", "-g", "-Wl,--build-id"])
            .arg(&source)
            .arg("-o")
            .arg(&library),
    );
    let id = omn::stack::build_id_of(&library).unwrap();
    let cache = dir.join("cache");
    let cached_dir = cache.join(id);
    std::fs::create_dir_all(&cached_dir).unwrap();
    let debug = cached_dir.join("debuginfo");
    checked(
        Command::new("objcopy")
            .arg("--only-keep-debug")
            .arg(&library)
            .arg(&debug),
    );
    checked(Command::new("strip").arg("--strip-all").arg(&library));
    let child = |mismatch: bool| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "local_symbols_use_only_matching_cached_debug_info",
                "--nocapture",
            ])
            .env("OMN_TEST_LOCAL_DSO", &library)
            .env("DEBUGINFOD_CACHE_PATH", &cache)
            .env_remove("DEBUGINFOD_URLS");
        if mismatch {
            command.env("OMN_TEST_MISMATCH", "1");
        }
        checked(&mut command);
    };
    child(false);
    // A valid ELF with another build id must not name this library's code.
    std::fs::write(&source, "static int another_worker(void) { return 42; }\nvoid *worker_address(void) { return another_worker; }\n").unwrap();
    let other = dir.join("other.so");
    checked(
        Command::new("cc")
            .args(["-shared", "-fPIC", "-g", "-Wl,--build-id"])
            .arg(&source)
            .arg("-o")
            .arg(&other),
    );
    std::fs::copy(other, debug).unwrap();
    child(true);
    std::fs::remove_dir_all(&dir).unwrap();
}

fn checked(command: &mut Command) {
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "{command:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
