//! Launching the observed program.
//!
//! A program launched to be measured has to be held at the starting line until
//! the collectors are attached. Otherwise a short program is half over before
//! anything is watching, and the allocations that happen during startup -- the
//! ones most likely to explain the memory curve -- are missing.
//!
//! `std::process::Command` cannot do this. It waits for the child's exec to
//! close a status pipe, so a child that stops before exec blocks the parent
//! forever. So the fork and the exec are done here, with a pipe used the other
//! way round: the child blocks reading it, the parent writes one byte when the
//! collectors are ready, and the child execs immediately after.
//!
//! The pid does not change across the exec, so an event attached before the
//! program exists follows it into the program.

use std::ffi::CString;
use std::io;

/// A program forked and waiting for permission to exec.
pub struct Gated {
    pid: i32,
    /// Written to once, to release the child.
    gate: i32,
    status: Option<i32>,
}

impl Gated {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    /// Lets the child exec.
    pub fn release(&mut self) {
        if self.gate >= 0 {
            let go = [1u8];
            // SAFETY: writing one byte to a pipe this process owns.
            unsafe {
                libc::write(self.gate, go.as_ptr() as *const libc::c_void, 1);
                libc::close(self.gate);
            }
            self.gate = -1;
        }
    }

    /// Whether the child has exited, without blocking.
    pub fn finished(&mut self) -> Option<String> {
        if let Some(status) = self.status {
            return Some(describe(status));
        }
        let mut status = 0;
        // SAFETY: waiting on a child of this process, without blocking.
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if result == self.pid {
            self.status = Some(status);
            Some(describe(status))
        } else {
            None
        }
    }

    /// Ends the child and reaps it, so the tool leaves nothing behind.
    pub fn terminate(&mut self) {
        if self.status.is_some() {
            return;
        }
        // SAFETY: signalling and reaping a child of this process.
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
            let mut status = 0;
            libc::waitpid(self.pid, &mut status, 0);
            self.status = Some(status);
        }
    }
}

impl Drop for Gated {
    fn drop(&mut self) {
        // A child still parked at the gate would wait forever.
        self.release();
        self.terminate();
    }
}

fn describe(status: i32) -> String {
    if libc::WIFEXITED(status) {
        format!("exit status {}", libc::WEXITSTATUS(status))
    } else if libc::WIFSIGNALED(status) {
        format!("signal {}", libc::WTERMSIG(status))
    } else {
        format!("status {status}")
    }
}

/// Forks a child that waits at a gate, then execs `argv` when released.
///
/// `environment` is added to the child's environment, which is how a preloaded
/// shim is told where to send what it sees.
pub fn gated(argv: &[String], environment: &[(String, String)]) -> io::Result<Gated> {
    let (program, _) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no program to launch"))?;

    // Everything the child needs is built before the fork: between fork and
    // exec only async-signal-safe calls are allowed, and allocating is not one.
    let c_program = CString::new(program.as_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "program name contains a NUL"))?;
    let c_argv: Vec<CString> = argv
        .iter()
        .map(|argument| {
            CString::new(argument.as_str())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "argument contains a NUL"))
        })
        .collect::<io::Result<_>>()?;
    let mut argv_pointers: Vec<*const libc::c_char> =
        c_argv.iter().map(|argument| argument.as_ptr()).collect();
    argv_pointers.push(std::ptr::null());

    let mut variables: Vec<CString> = std::env::vars()
        .filter(|(name, _)| !environment.iter().any(|(added, _)| added == name))
        .chain(environment.iter().cloned())
        .map(|(name, value)| {
            CString::new(format!("{name}={value}")).unwrap_or_else(|_| CString::new("").unwrap())
        })
        .collect();
    variables.retain(|variable| !variable.as_bytes().is_empty());
    let mut environment_pointers: Vec<*const libc::c_char> =
        variables.iter().map(|variable| variable.as_ptr()).collect();
    environment_pointers.push(std::ptr::null());

    let mut pipe_fds = [0i32; 2];
    // SAFETY: `pipe` fills two file descriptors into an array we own.
    if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (read_fd, write_fd) = (pipe_fds[0], pipe_fds[1]);

    // SAFETY: this process is single threaded at launch time, and the child
    // path below calls only async-signal-safe functions before exec.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = io::Error::last_os_error();
        unsafe {
            libc::close(read_fd);
            libc::close(write_fd);
        }
        return Err(error);
    }

    if pid == 0 {
        // Child: wait at the gate, then become the program.
        unsafe {
            libc::close(write_fd);
            let mut go = [0u8; 1];
            // A short read or an interrupted one still means "go": the parent
            // only ever writes when it is ready, and closing the pipe without
            // writing means it gave up.
            libc::read(read_fd, go.as_mut_ptr() as *mut libc::c_void, 1);
            libc::close(read_fd);
            libc::execvpe(
                c_program.as_ptr(),
                argv_pointers.as_ptr(),
                environment_pointers.as_ptr(),
            );
            // Only reached when exec failed; 127 is what a shell reports for
            // "command not found", and the parent surfaces it as the exit.
            libc::_exit(127);
        }
    }

    // SAFETY: closing the end of the pipe this process does not use.
    unsafe { libc::close(read_fd) };
    Ok(Gated {
        pid,
        gate: write_fd,
        status: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_waits_at_the_gate_until_it_is_released() {
        let mut child = gated(&["/bin/true".to_owned()], &[]).expect("fork");
        // Nothing released it yet, so it cannot have run.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(
            child.finished().is_none(),
            "the child ran before it was released"
        );

        child.release();
        for _ in 0..100 {
            if let Some(status) = child.finished() {
                assert_eq!(status, "exit status 0");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the child never finished after being released");
    }

    #[test]
    fn a_program_that_does_not_exist_exits_with_the_shell_convention() {
        let mut child = gated(&["/nonexistent/program".to_owned()], &[]).expect("fork");
        child.release();
        for _ in 0..100 {
            if let Some(status) = child.finished() {
                assert_eq!(status, "exit status 127");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the child never finished");
    }

    #[test]
    fn the_child_environment_carries_what_was_added() {
        let marker = "OMN_LAUNCH_TEST";
        let mut child = gated(
            &[
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                format!("test \"${marker}\" = ok"),
            ],
            &[(marker.to_owned(), "ok".to_owned())],
        )
        .expect("fork");
        child.release();
        for _ in 0..100 {
            if let Some(status) = child.finished() {
                assert_eq!(
                    status, "exit status 0",
                    "the variable did not reach the child"
                );
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the child never finished");
    }
}
