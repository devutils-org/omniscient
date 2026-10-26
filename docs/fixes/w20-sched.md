# Stream sched, twentieth wave: the sched collectors the views found short

Branch `feat/w20-sched`, on `444900b`. Items 8 (sched) and 11 (`sched.lock`,
`sched.futex` following descendants) of `docs/dev/STATE.md`, and the two example
headers of item 10.

## 1. What landed

- **`sched.wakeup` names both ends** (`src/bpf/wakeup.rs`,
  `src/collect/sched/wakeup.rs`). `comm` and `pid` stay the wakee's (the event
  is keyed on it); beside them `wakee_comm`, and on a task's wakeup
  `waker_comm`, read with `bpf_get_current_comm` in the waker's context.
- **An interrupt's wakeup says which interrupt, and where it was served.**
  `served_in_tid` and `served_in_comm` are the task current when the
  interrupt issued the wakeup (the waker's tid before the program zeroes it).
  `irq` (hard interrupt number, as `/proc/interrupts` numbers it) and
  `softirq` (the vector's kernel name: `TIMER`, `NET_RX`, `BLOCK`, ...) come
  from a per-cpu two-slot map kept by four small programs on
  `irq_handler_entry`/`exit` and `softirq_entry`/`exit`
  (`InterruptNumbers`). Each slot holds the number plus one, so 0 is "none".
  If those programs cannot load, the run says so in a note and the wakeups
  carry no `irq`/`softirq`. The record grows from 48 to 80 bytes.
- **`sched.offcpu`'s stamp is described as it happens.** The catalog caveat
  now says a row is stamped at the drain that closed its window, and the
  blocks it adds up ended in `(ts_ns - window_ns, ts_ns]`. That is what the
  code does and what the lanes and heatmap views already read. The stamp
  itself is unchanged.
- **`sched.lock` and `sched.futex` follow a process target's children**
  (`src/bpf/sched_{lock,futex,common}.rs`, `src/collect/sched/{lock,futex,mod}.rs`).
  `push_current_followed_filter` compares the root as a constant and looks
  any other thread group up in the same descendants map `sched.offcpu` and
  `sched.wakeup` use. Each collector symbolizes the followed processes every
  tick (`refresh_followed_symbols`), drops the ones that left a tick later,
  and adds a note about any process the map had no room for. The catalog's
  raw_tp backends for both now require `sched_process_fork`/`free`, and
  `sched.lock` accepts `launch` (`bpf_only`), which it did not before.
- **The example headers** (`examples/sched-round-trips.yml`,
  `sched-half-blocked.yml`): the N blocks and N run-queue waits, and the
  182.5 ms, are now said to be on the driver's row. The responder subshell is
  followed too, so the run's totals are about 2N.

## 2. Shown, and reasoned

Shown (on this machine, with the capabilities, `target/debug/omn` after
`make`):

- `comm` was always the **wakee's**. It is read from the woken
  `task_struct` (`sched_waking`'s argument), and a launched run shows it:
  `wakee_tid=1116928 comm=omn waker_comm=omn` is the launched child before it
  executes, and later `comm=sh ... waker_comm=sleep`. The nineteenth wave's
  timebar finding ("a wakeup of `sqlite3` reads `comm: omn`") is a launched
  program before its `exec`, not the waker's name. The two named fields
  now make the question unnecessary.
- `each_child_that_exits_is_named_as_the_one_that_released_the_shell` now also
  checks that `comm == wakee_comm` on every line, and that a task's wakeup
  carries `waker_comm` and no `served_in_tid` while an interrupt's carries the
  reverse. It also checks for at least five wakeups of `sh` by `sleep`.
- `an_interrupts_wakeup_names_an_interrupt_the_kernel_numbers`: in a 1 s
  machine-wide run, every `irq` is a numbered line of `/proc/interrupts` and
  every `softirq` is one of the ten vectors. A manual run named 66 (`nvidia`)
  and 71-73 (`nvme0q3-5`); one test run counted 154 hard and 177 soft. Since
  the review round both sides are asserted present: see "Review".
- `a_childs_futex_wait_is_the_launched_programs`: a 500 ms lock hold inside
  `python3`, run as a child of a launched `sh -c '...; true'`. The main
  checkout's binary reports **0 events**, and this branch reports the one
  long wait, `close_to` 500 ms.
- `a_launched_programs_lock_waits_are_its_childrens`: twelve subshells append
  to one file, and the waits come from two or more processes. The root shell
  takes no inode lock itself.

Reasoned, not shown:

- Some hard-interrupt wakeups have no `irq`: those from the local APIC timer
  (most `sleep` endings: `sleep ... waker_context=hardirq` with no `irq` in
  the launched run above), from IPIs, and a threaded handler's wakeup of its
  own thread. The kernel issues that last one after `irq_handler_exit`. The
  catalog caveat says this. Reading `__irq_regs` and the vector table instead
  would cover them, but it is x86-specific and fragile; not done.
- Hard handlers do not nest on a cpu, and a softirq does not nest on its cpu,
  so one slot each is the whole state. The exit programs are attached before
  the entries, so a number cannot be left behind at attach time.
- The cost added to `sched.wakeup` is one `get_current_comm` per wakeup,
  two map lookups per interrupt wakeup, and four tiny programs on every
  interrupt on the machine. It is not measured; see 5.

## 3. Tests

- `cargo test --lib`: 1525 passed. This includes the new
  `an_interrupt_is_named_only_in_its_own_context` and
  `the_interrupt_programs_set_and_clear_one_slot`, and the record-offset test
  extended to the five new fields.
- `cargo test --test scenario_sched --test family_sched`: 55 + 17 passed,
  **with the capabilities** (after `make`). A first pass ran after a catalog
  edit had relinked the binary, and so ran without them: one test failed
  there (`a_source_with_no_collector...`, a pinned `bpf:uprobe` refused for
  want of `CAP_BPF`), and it passed once `make` had granted them again.
- `cargo test --test cli`: 26 passed (the catalog changed).
- `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check`: clean.

## 4. What I could not do, and why

- **`irq` on timer and IPI wakeups**: see 2. They are named by context alone
  (`waker_context: hardirq`).
- **`docs/scenarios/sched.md` and `docs/families/join.md`** say
  `sched.offcpu` stamps a row at its first wait. Neither file is this
  stream's (see 7).
- **A `sched.lock` example on a launched target**: `examples/sched-lock-*.yml`
  still use `--system` narrowed by `comm`, which is still correct. A launched
  form is now possible (the new test is one).

## 5. Privileged runs wanted

None beyond the capabilities `make` grants: every test above ran with them.
Once, alone on the machine, the cost table should be measured again. It now
includes `sched.wakeup`'s four interrupt programs, which run on every
interrupt on the machine:

```console
scripts/bench.sh backends   # the cost table; sched.wakeup's `cost: med` re-checked
```

## 6. Decision lines

- **An interrupt's wakeup names the task it was served on as
  `served_in_tid`, never as the waker.** For a softirq served in
  `local_bh_enable` that task is the cause. For an interrupt that only landed
  on it, the task is not. The reader decides, and the context field says which
  case it is. Cost of leaving it open: none; the name says what was measured.
- **`sched.offcpu` keeps its drain stamp.** The caveat is changed, not the
  stamp, because the views already read rows as ending at `ts_ns`. Stamping
  at the first wait, as the other sched sources do, would need a field in the
  program's value and a change in the lanes and heatmap readers. It is open
  only if a join needs it.

## 7. Needs from another stream

- **The coordinator / docs** (`docs/families/join.md:748`): the sentence
  "`sched.futex` and `sched.offcpu` ... stamp a row when its first wait
  began" is true of `sched.futex`, not of `sched.offcpu`, which stamps at
  the drain (catalog caveat, `src/collect/sched/offcpu.rs` `drain_into`).
  `stamped: start` on a join to `sched.offcpu` therefore covers
  `[ts, ts + span]` where the blocks lie in `[ts - window_ns, ts]`.
- **docs** (`docs/CATALOG.md`'s sched section): `sched.wakeup`'s new fields
  (`wakee_comm`, `waker_comm`, `served_in_tid`, `served_in_comm`, `irq`,
  `softirq`); `sched.lock` launchable; lock and futex following children.
- **view** (the timebar's and wake's names): `wakee_comm`/`waker_comm` are
  now explicit, and `comm` is the wakee's as before. The wake view's want
  (`irq` on `hardirq`, `served_in_tid` on `softirq`) is served, minus the
  timer/IPI case.
- **The coordinator** (`docs/dev/STATE.md` item 8/11, `docs/scenarios/sched.md`): the
  timebar finding that `comm` is the waker's is withdrawn (section 2), and the
  scenarios file's offcpu stamping line should match the catalog.

## Review

- **The hard-interrupt side of
  `an_interrupts_wakeup_names_an_interrupt_the_kernel_numbers` held
  vacuously.** Fixed. The test now drives its own device interrupts: a
  python reader opens a 4 MiB scratch file under `target/tmp` with `O_DIRECT`
  and reads it for the whole window, so every read is a completion of the
  disk under `target/`. It asserts `hard > 0` (and that the reader read)
  alongside `soft > 0`. On this btrfs machine the reader itself is woken by
  a kworker, because btrfs queues data-read completions to its end-io
  workqueue; the kworker is what the nvme handler wakes, with `irq=69`
  (`nvme0q1`) and the like. On ext4 the reader would be woken directly. A
  filesystem that refuses `O_DIRECT`, or no `python3`, falls back to holding
  the values to the table and says so.
  Shown: 40400 hard and 10731 soft over 79959 reads; and with the collector
  mutated to never push `irq`, the test fails (`0 wakeups named a hard
  interrupt`, "71135 direct reads from the disk, and no wakeup named a hard
  interrupt"), then passes again with the collector restored.
