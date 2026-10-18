# Stream triage, the twenty-fifth wave

Branch `feat/w25-triage`. Done sequentially by one agent, in the main
checkout.

## What landed

1. **The agent's way's job by `ppid`** (`tests/cases.rs`, `read_mcp`): the
   rest of the job is the busy tasks whose process runs under the chosen
   child's parent, read off the raw events' `pid` and `ppid` (`Processes`),
   with triage's rule for `descends`: yes when the parents reach it, no at
   the main process or pid 1, unknown (left out) when they stop. With no
   `ppid` in the run, every other busy task, the upper bound it was.

## Shown vs reasoned

- A synthetic read-back (`the_agent_s_job_is_what_runs_under_the_chosen_child_s_parent`):
  a sibling of the chosen child counted, a process of the machine and one
  whose parent was never seen left out; without `ppid`, all three.

## Not done (privileged case runs, nothing to change before them)

1. **`make-sqlite`'s critical path with `ppid`**: the person's way already
   restricts the job by `ppid` (`Job::under`), and `sched.offcpu` and
   `sched.runqlat` carry it; the case needs CAP_BPF to run. Its open
   finding should then fail as passing:
   `OMN_CASES_ONLY=make-sqlite cargo test --test cases -- --nocapture` after
   `make`.
2. **`sqlite3-durability`'s `every-commit-waits-for-the-disk`** (blocked 24
   to 27 ms over `schedstat` where 8 and 12 are allowed, a `sometimes` open
   finding): no cause to change without a privileged rerun on a quiet
   machine; `OMN_CASES_ONLY=sqlite3-durability`.

## Tests

`--test cases --test triage`, `--lib triage`, clippy, fmt.
