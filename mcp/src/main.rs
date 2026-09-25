//! `omn-mcp`: a model reads what omn already writes.
//!
//! `ROADMAP.md` phase 11. An MCP server over stdio -- one JSON-RPC message per
//! line in, one per line out -- exposing objects that exist: the catalog, a
//! run's resolved file and results document, and the capability sweep. It is
//! a second surface on those objects, the way a session file is a second
//! surface on a `Request`, and not a report generator: nothing is measured,
//! summarised or modelled here, and every number handed out travels with the
//! fields that say what it is missing.
//!
//! It reads; it does not run. The one tool that runs a session exists only
//! when the person starting the server passes `--allow-run`, is bounded by
//! what they passed, and refuses a session that would launch a program.
//! `docs/mcp.md` has why.

mod binary;
mod evidence;
mod runs;
mod server;
mod session;

use std::io::{BufRead, Write};
use std::path::PathBuf;

const USAGE: &str = "\
usage: omn-mcp [--dir DIR] [--omn PATH] [--allow-run DURATION]

An MCP server over stdio for what omn writes.

  --dir DIR             where runs are read from (directories `omn -o` wrote),
                        and where an allowed run writes; default: the current
                        directory
  --omn PATH            the omn binary that answers every question about this
                        machine; default: `omn` beside this executable
  --allow-run DURATION  enables `run_session`, for sessions over targets that
                        already exist, each at most DURATION long (`10s`).
                        Absent, the tool refuses every call and says this.
";

fn main() {
    let config = match server::Config::from_args(std::env::args().skip(1)) {
        Ok(config) => config,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    let mut server = match server::Server::new(config) {
        Ok(server) => server,
        Err(message) => {
            eprintln!("omn-mcp: {message}");
            std::process::exit(1);
        }
    };

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle_line(&line) {
            let mut text = reply.to_string();
            text.push('\n');
            if stdout.write_all(text.as_bytes()).is_err() || stdout.flush().is_err() {
                break;
            }
        }
    }
}

/// Resolves a path given on the command line against the current directory.
fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}
