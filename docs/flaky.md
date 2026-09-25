# The flaky tally, audited

The twelfth wave's audit of every test named in the flaky tally, read from the
code with nothing built or run, then corrected where the coordinator measured: `docs/open-findings.md`
findings 41 to 46 (including "The tally, for the eighth wave" and the eighth wave's
"Two tests that fail under a parallel workspace run", the seventh wave's
`scenario_io` note and the `scenario_cpu` note), every "For the flaky tally" section
(tenth, eleventh and twelfth waves, the coordinator's included), finding 48, and
`docs/scenarios/io.md` "Two flaky tests seen from here". Nothing was built or run. A
class is a reading of the code, and where the recorded failure message does not settle
it, the paragraph says what would.

Classes: **(a)** it asserts what the machine was doing, not what the workload did;
**(b)** a timing threshold or delay calibrated on a quiet machine; **(c)** a real tool
defect that load exposes; **(d)** cannot tell from reading. Owner is the twelfth
wave's, from `BRIEF.md`'s table. "nobody" means the file is not listed there.

| # | Test | file:line | Class | Owner | One-line fix |
|---|---|---|---|---|---|
| 1 | `one_known_number_of_round_trips_is_found_by_four_sources` | tests/scenario_sched.rs:340 | a (+c) | D | Switches: a floor of N, plus `switches - blocks` checked against the kernel's `nivcsw` delta. Wakeups: the pair's two top wakee series, as #2 already does |
| 2 | `a_workload_that_is_half_blocked_measures_as_half_blocked` | tests/scenario_sched.rs:443 | c (finding 48), with a leftover (a) | D | Defect: `src/bpf/wakeup.rs` should not credit a wakeup made in interrupt context to the task it interrupted. Share: use `total_ns - queued_ns` |
| 3 | `an_overflowing_wakeup_ring_says_what_it_could_not_hold` | tests/scenario_sched.rs:1085 | a (+c) | D | In the no-loss branch, assert only the floor `seen >= expected*(1-tol)`, or count only the pair threads' wakee series |
| 4 | `a_blocking_stack_names_the_call_that_blocked` | tests/scenario_sched.rs:620 | c, fixed | D | Already fixed: the target is compared before the stack walk (`docs/families/drains.md` §2). Take it off the tally if it stays green |
| 5 | `a_task_moved_between_two_cpus_is_counted_once_per_move` | tests/family_sched.rs:832 | b | D | Replace the fixed 800 ms attach delay with a wait for the run's "attached" signal, or keep moving the task until the run ends and count the moves inside its window |
| 6 | `tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them` | tests/family_sched.rs:597 | a | D | Check each task's `total_ns` against its own `/proc/<tid>/schedstat` `run_delay` delta, not against the share of the window |
| 7 | `what_the_signal_ring_could_not_hold_is_counted_and_closes_against_what_was_sent` | tests/scenario_proc.rs:1645 | b | W | Drop `late_ticks == 0`. Instead assert that the "ring was full" reason carries the drop count. Replace `ATTACH_MARGIN` with a readiness signal |
| 8 | `per_cpu_utilisation_reports_one_series_for_each_core_asked_for` | tests/family_cpu.rs:244 | c | W | Change `busy = 1.0 - idle/total - iowait/total` in `src/collect/cpu/util.rs:316` to `(total - idle - iowait) as f64 / total as f64`. The current form goes to -2.8e-17 on an idle core that has iowait |
| 9 | `a_known_number_of_retired_instructions_is_counted_as_that_number` | tests/scenario_cpu.rs:349 | a (to confirm, d) | W | Count user mode only for the loop's difference, or carry the kernel-mode share (a second `:k` counter) as slack |
| 10 | `a_half_blocked_task_decomposes_into_what_the_kernel_says_it_did` | tests/cross_compute.rs:567 | a | W | Replace the 0.35..0.65 wall-clock share with the driver's blocked time less its queue wait, bracketed by the responder's schedstat on-cpu time; replace `short.is_none()` with `short.is_some() == (dropped > 0)` |
| 11 | `collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned` | src/collect/mod.rs:3662 | b | R | Stop on 200 ms of spun cpu (`spun_ns`), not a 200 ms wall deadline |
| 12 | `collect::tests::weighing_a_run_costs_a_fraction_of_the_run` | src/collect/mod.rs:3812 | b | R | Time the weighings on `CLOCK_THREAD_CPUTIME_ID`, not the monotonic clock, or move the 1 % bound to the bench |
| 13 | `budget::what_the_tool_says_it_spent_moves_with_how_often_it_was_asked_to_read` | tests/budget.rs:94 | b | R | Compare cost per read actually made (the run's read count), not the fixed ratio `often > rarely * 5` |
| 14 | `budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes` | tests/budget.rs:187 | b | R | Make the polling dominate the startup (a 2 s window, or poll@100us), or carry a measured startup spread as slack. Two runs' startups do not cancel under load |
| 15 | `triage::a_detailed_layer_over_its_budget_is_let_go_and_the_escalation_goes_on` | tests/triage.rs:1426 | a | R | Make the lock phase lock-bound whatever the run queue does (a longer `HOLD_US`, a longer phase), or assert the decision against the cheap pass's own readings |
| 16 | `scenario_net::every_segment_the_kernel_resent_is_one_this_run_saw` | tests/scenario_net.rs:2794 | a | D | Read `RetransSegs` after the socket is aborted, not before exit, and identify the socket by (sport, dport, netns), not by port alone |
| 17 | `scenario_net::a_connection_is_joined_from_its_handshake_to_its_close_by_the_cookie_alone` | tests/scenario_net.rs:3899 | a (d) | D | Match by (sport, dport = the listener's port), or by the test's pid, not by `sport` on a system target |
| 18 | `scenario_io::the_page_cache_sessions_say_what_their_headers_predict` | tests/scenario_io.rs:4423 | a | D | Carry `losses.dropped` (the kernel's skips) as slack on the exact 4096, and have the workload `mincore` the file before the warm read and print what was evicted |
| 19 | `scenario_io::the_tracer_and_the_kernels_own_counter_see_the_same_requests` ("0 bytes traced") | tests/scenario_io.rs:2371 | d (the audit's (c) refuted, below) | D (test); nobody (`src/bpf/io.rs`) | Not the kblockd submitter: measured, every request credited to the reading thread under 32 direct writers. What makes a total miss is still unknown; log the run's `comm` counts and `losses` when it fails |
| 20 | `scenario_mem::every_mode_and_emit_of_the_sampled_sources_is_served_or_refused_by_the_right_code` | tests/scenario_mem.rs:3175 | c | D (test); R (`src/emit.rs`) | `emit::stack` calls `value_of` where it should call `measured`, so an untimed IBS load is `NotNumeric` (exit 1). Count it as `unmeasured` |
| 21 | `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter` | tests/scenario_mem.rs:1095 | a | D | Take the meminfo bracket as min/max over reads interleaved with each round, not before and after the session. Or make the huge pages the workload's own |
| 22 | `scenario_mem::compaction_is_seen_where_the_kernel_says_it_happened_and_skipped_loudly_where_it_did_not` | tests/scenario_mem.rs:2703 | a, fixed | D | Already fixed (a kcompactd wake is no longer taken as compaction). Off the tally |
| 23 | `collect::mem::wss::tests::a_process_that_holds_much_and_touches_little_reports_the_little` | src/collect/mem/wss.rs:321 | a | D | The walker counts its walks in the handshake page. Wait for two walks after the clear instead of sleeping 300 ms |
| 24 | `scenario_gpu::a_resident_kernel_separates_from_an_idle_device_on_occupancy_and_on_power` | tests/scenario_gpu.rs:938 | a | V | Re-check `device_to_ourselves()` after the busy run and skip with the numbers, or assert per-process NVML utilisation, not device-wide |
| 25 | `cross_io::the_crossing_sessions_run_and_say_what_they_found` | tests/cross_io.rs:1493 | b | nobody | Replace `LEAD_IN` (1 s) with a readiness signal, and run the two sessions one after the other rather than side by side on one disk |
| 26 | `join_engine::the_resident_size_decomposes_into_live_and_retained_with_the_residue_the_program_predicts` | tests/join_engine.rs:732 | d | nobody | Log the heap requests' `losses.dropped` and the last reading's time against `ARENA`. A shim drop, or a late last poll, would decide it |
| 27 | `encoders::a_known_allocation_lands_on_the_chrome_trace_at_the_time_it_was_made` | tests/encoders.rs:695 | b | nobody | The workload prints its own `CLOCK_MONOTONIC` at the allocation, and the test splits the series there, not at 300 ms after the run began |

**Count: (a) 13, (b) 8, (c) 4, (d) 2**, after the coordinator's measurement of #19. Of the 27, two are already fixed (#4 and
#22), and #2's recorded half is fixed in the test, though the tool defect behind it
(finding 48) is not.

---

## Evidence, per test

**1. `one_known_number_of_round_trips_is_found_by_four_sources`.**
`assert!(close_to(switches, ROUND_TRIPS), "{ROUND_TRIPS} round trips are
{ROUND_TRIPS} switches out of the cpu, saw {switches}")`. The recorded failure was 2643
for 2000. `sched.switches` is perf's software context-switch count, which counts
preemptions as well as blocks. The sibling test `perf_counts_the_blocks_...`
(scenario_sched.rs:404) says so: "measured inside this suite ... perf saw 2196 and the
tracepoint 1972". The load preempts the driver, so the count describes the machine.
What the workload creates is N *voluntary* switches. The fix is to assert the floor,
plus `switches - blocks` against `/proc/<pid>/status` `nonvoluntary_ctxt_switches`.
The same test also asserts `close_to(wakeups, 2*ROUND_TRIPS)` over `events_seen`,
which finding 48 inflates in the same way. It should read the pair's two wakee series,
as #2 now does. The sibling test's `close_to(switches, ...)` has the same exposure and
is next in line for the tally.

**2. `a_workload_that_is_half_blocked_measures_as_half_blocked`.** Finding 48: 1584
wakeups against 1200 ±25 %, because `sched.wakeup` takes the waker from
`bpf_get_current_pid_tgid`, which inside a hard or soft interrupt is whatever task
the interrupt landed on. The test now asserts the pair: "Not the run's total, which a
busy machine inflates". The defect in `src/bpf/wakeup.rs` (D's) is still open.
Leftover (a): `(0.35..=0.65).contains(&blocked_share)` takes `total_ns` from
`sched.offcpu`, and `src/bpf/offcpu.rs:189-195` says that `total_ns` includes
`queued_ns`, the run queue wait after the wakeup. Under load that wait grows, and so
does the responder's wall-clock spin, which is the driver's block. The halves are
equal in cpu time, not in wall time. Using `total_ns - queued_ns` against the on-cpu
half removes the first effect.

**3. `an_overflowing_wakeup_ring_says_what_it_could_not_hold`.** Under load the
python workload is slower than the ring, so `lost == 0` and the test takes
`assert!(close_to(seen, expected), "nothing was dropped and the count is still wrong
...")`: a two-sided ±25 % around 80 000. Its own doc comment names the upper-side
cause: "a contended interpreter lock adds wakeups of its own". Finding 48's
interrupt-credited wakeups add more. Both are real wakeups of the target, so the only
thing the workload guarantees is the floor. `docs/fixes/stacks.md:308` records the
failure without its message. It should be confirmed as the no-loss branch.

**4. `a_blocking_stack_names_the_call_that_blocked`.** (c), fixed. The program walked
every blocking stack on the machine before narrowing to the target, and a loaded
machine filled the 4096-bucket stack map (`docs/families/drains.md` §2). The target is
now compared before the walk, and
`a_targets_blocking_stack_is_named_while_another_process_floods_the_stack_map` makes
the load deterministic. Nothing to do but take it off the tally if it stays green.

**5. `a_task_moved_between_two_cpus_is_counted_once_per_move`.** `std::thread::sleep(
Duration::from_millis(800))` and the comment beside it: "Long enough for the program
to be loaded and attached before the first move". Then `assert!(counted >= MOVES as
u64, ...)`. The recorded failure is 0 of 20. Finding 46's neighbour measured fentry
attaches serialising machine-wide, at 2.2 to 2.4 s each under six at once. The fixed
delay is a quiet-machine calibration.

**6. `tasks_sharing_one_cpu_wait_the_share_the_scheduler_owes_them`.** `assert!(alone
< 0.25, ...)` and `assert!((0.45..0.90).contains(&contended), ...)`, as a share of a
1 s window on a pinned cpu. The idle check (`busy > 0.2` → skip) runs once, before
the load. Anything else the machine schedules on that cpu during the second is in the
wait. The kernel keeps the exact reference per task: `run_delay` in
`/proc/<tid>/schedstat`, which `cross_compute.rs` and `join_engine.rs` already use.
The separation "three pinned tasks wait at least the other two's runtime" also holds
under load.

**7. `what_the_signal_ring_could_not_hold_is_counted_and_closes_against_what_was_sent`.**
The recorded failure is "6 late ticks of a 10 ms tick", against `assert_eq!(result
["losses"]["late_ticks"].as_u64(), Some(0), "a full ring is a drop, not a tick that
ran late")`. The premise in the comment ("a ring-backed source has no clock to run
late") is not what the runner does. `src/collect/mod.rs:2707` counts any round that
outran its interval as a late tick, for every collector, and the drain of a 300 000
signal storm under load outruns 10 ms. That lateness is also why the ring dropped, so
the branch contradicts itself under exactly the condition it runs in. There is a
second exposure: `ATTACH_MARGIN` (700 ms, scenario_proc.rs:1261) is a fixed delay
before the workload starts. If the attach is late, the `dropped == 0` branch's
`assert_eq!(counted, SENT, ...)` fails.

**8. `per_cpu_utilisation_reports_one_series_for_each_core_asked_for`.**
`assert!((0.0..=1.0).contains(&value), "`{bound}` utilisation of cpu {cpu} is
{value}, which is not a share of anything")` on the min and max of `busy`.
`src/collect/cpu/util.rs:316` computes `Attr::float("busy", 1.0 - share(delta.idle) -
share(delta.iowait))`. For a core that did nothing but idle and wait on I/O,
`1.0 - 90/100 - 10/100` is `-2.78e-17` in f64 (checked). Iowait on cpu 0 or 1 is what
a neighbour's disk-heavy suite produces, and that is why this fails under load. The
failure message is not recorded, so to confirm, print `min` on the next failure. The
fix is integer arithmetic before the division.

**9. `a_known_number_of_retired_instructions_is_counted_as_that_number`.**
`assert!(error.abs() < 0.005, ...)` on `measured - startup` against 10^9. The failures
overshoot by +0.69 %, and the excess scales with the run length (13.85 M against
6.97 M). `cpu.instructions` opens with `exclude_kernel: false` by default
(`src/perf.rs:122`), so every interrupt handled in the task's context while the loop
runs is counted in the loop. A longer run gets more of them, and a loaded machine
raises more of them. That is the machine, not the workload. A run of the same pair
with a user-only counter would decide it: if the +0.69 % disappears, it was kernel
mode. It also fails once in three on an idle machine, so part of it may be (d).

**10. `a_half_blocked_task_decomposes_into_what_the_kernel_says_it_did`.** The kernel
cross-checks (schedstat on-cpu, `run_delay`, voluntary switches) are well built and
carry their resolution. The two load-sensitive assertions are `(0.35..=0.65).contains(
&blocked_share)`, with `off_cpu_ns` including the queue wait (as in #2), and
`assert!(decomposition.short.is_none(), "a clean run gains no mark ...")`, which never
checks that the run *was* clean. Since the lossy-profile fix (8b57293), a run that
drops samples under load gets the mark, correctly. The assertion should be that the
mark appears exactly when `dropped > 0`.

**11. `collect::tests::what_a_run_reports_spending_is_the_cpu_it_actually_burned`.**
`let deadline_ns = monotonic_ns() + 200_000_000;` then `if spun_ns < 150_000_000.0 {
NOTHING_WAS_BURNED }`. It drives to a wall deadline and demands 150 ms of cpu, which a
child sharing a loaded machine does not get in 200 ms of wall time. `docs/fixes/
catalog.md` / finding 44's "power 5" already names it: "driving to a wall deadline
instead of a cpu one". The `until` closure should read `spun_ns`.

**12. `collect::tests::weighing_a_run_costs_a_fraction_of_the_run`.** `assert!(overhead
.measurement_ns * 100 < overhead.wall_ns, ...)`. The recorded failure is 12.3 ms of
605 ms. `measurement_ns` is summed from `monotonic_ns()` around each `getrusage`
(`src/collect/mod.rs:2088-2191`), so a weighing that is preempted counts the
preemption as its cost. The honest amount is the thread's cpu time, or the bound
belongs to the bench.

**13. `budget::what_the_tool_says_it_spent_moves_with_how_often_it_was_asked_to_read`.**
`assert!(often_ns > rarely_ns * 5, ...)`. poll@1ms over 500 ms is only 500 reads when
the runner is scheduled on time. A late round re-bases the schedule
(`mod.rs:2708-2710`), so under load the fast run makes far fewer reads than it was
asked for. The workload's property is the cost of each read the run made, and the run
counts both reads and late ticks.

**14. `budget::the_extra_cpu_a_faster_poll_costs_is_the_extra_cpu_the_shell_observes`.**
`observed_extra_ns > reported_extra_ns * 0.4 - slack && observed_extra_ns <
reported_extra_ns * 3.0 + slack`. The `times` quantum is already carried
(`TIMES_QUANTUM_NS`), which is the right idea. The unguarded part is the premise that
"the startup is the same in both". Under load, the catalog load and probe of two
separate runs differ by more than 40 ms of polling, and `observed` covers the startup
while `reported` does not. Either the polled share must be made to dominate, or one
run's startup spread must be measured and carried.

**15. `triage::a_detailed_layer_over_its_budget_is_let_go_and_the_escalation_goes_on`.**
`assert!(run.attached("futex").is_some(), "the escalation went on deciding, and named
the lock layer after the shed")`. The engine names a layer only when one state holds
`ns * 2 > interval_ns && ns > next` for `READINGS = 2` readings of 200 ms in a row
(`src/triage/escalate.rs:157-166`, 59-63). The Lock layer is `sleeping_ns`, and the
Queue layer is separate. In the lock phase (800 acquisitions of a 2 ms hold), each
wakeup is followed by a run queue wait that the load lengthens. When the load pushes
queue plus cpu above half, nothing is named and futex never attaches. The tool is
being truthful, since the main thread was partly queued. The workload has to make the
lock the dominant state by margin, not by a quiet machine. The engine's own readings
are in `run.notes("blocked")`, and they would show the classification.

**16. `scenario_net::every_segment_the_kernel_resent_is_one_this_run_saw`.** "4 against
3", from `assert_eq!(mine.len() as u64, kernel_counted, ...)`. The reference is read
at `time.sleep(1.1)` and the socket then lives on until the interpreter exits, which
under load is later. Backoff from 200 ms puts retransmissions at about 0.2, 0.6 and
1.4 s, so one that fires after the read and before the close is seen by `omn` (1.8 s
window) and not by the counter. The reference's resolution is its read time. `mine`
also filters on `sport` alone for a system target, and a second namespace's
connection on the same ephemeral port is possible under a parallel suite.

**17. `scenario_net::a_connection_is_joined_from_its_handshake_to_its_close_by_the_cookie_alone`.**
The failure message is not recorded (`docs/fixes/silent.md:285`, "once"). Both
sources are machine-wide (`target: system`), and the test picks events with
`.find(|event| fx_u64(event, "sport") == u64::from(*port))`. Any other loopback
connection in the 1.5 s window that reuses a client port matches first, so
`open.get("cookie") == close.get("cookie")` compares two connections. The other
exposure is `mine.len() >= 3` against two fentry attaches serialised under load in a
1.5 s window. Printing the failing assertion decides which. Matching on (sport, dport)
removes the first.

**18. `scenario_io::the_page_cache_sessions_say_what_their_headers_predict`.** The
assertions are exact: `assert_eq!(number(&heaviest["total"]), 4096.0, ...)` and
`assert_eq!(number(&windows["min"]), -4096.0, ...)`. Under memory pressure from other
builds, pages of the file can be reclaimed in the 0.3 s between the two reads, and the
warm read then fills some of them. Raw-tracepoint programs are also skipped by the
kernel under interrupt load (finding 44: 766 and 1237 skipped runs in one `scenario_io`
run). Those skips are now counted in `losses.dropped`, but this test does not carry
them. Both are the machine.

**19. `scenario_io::the_tracer_and_the_kernels_own_counter_see_the_same_requests`.**
Seventh wave, finding 41's note: "`0 bytes traced for the 16777216 the workload
read`". The 1 % shortfalls are now covered by `dropped * PAGE` slack. A total miss is
not a skip, though. `src/bpf/io.rs:616-634` attributes an unpaired `io.size` request
with "the submitter is the current task: a `block_rq_issue` for a read runs in the
context that asked for it". That is not true when blk-mq defers dispatch to kblockd,
which it does when the hardware queue is busy, and a neighbour driving the disk makes
it busy. Every request is then the kworker's and is filtered out. This is finding
48's shape at the block layer. To decide it, rerun under disk load with the
submitter filter off and count the `comm` of the 4096 requests.

**Measured by the coordinator, and the hypothesis does not hold here.** The
exact workload of the scenario (4096 `O_DIRECT` preads of a page, from a thread,
the page cache evicted first), traced with `omn io.size -m trace -e raw`: quiet,
4096 of 4096 requests credited to `Thread-1 (reads`; beside four `dd
oflag=direct` writers, the same; and in three runs beside thirty-two of them,
4096 of 4096 each time, with the two requests the kernel skipped reported as
dropped. On this NVMe device, with its deep queues, blk-mq did not defer one
dispatch to kblockd. The submitter-at-issue attribution may still be wrong on a
shallow-queued device, but it is not what made this test miss everything here,
and the cause stays open (class d).

**20. `scenario_mem::every_mode_and_emit_of_the_sampled_sources_is_served_or_refused_by_the_right_code`.**
`mem.latency sample stack` exits 1 with "`latency_cycles` is not a numeric
attribute". `src/collect/mem/ibs.rs:549` omits `latency_cycles` for an untimed load
(an L1 hit on family 17h) by design, and `src/emit.rs` has `measured()` to count such
events as `unmeasured`. But `fn stack` (emit.rs:1193) calls `value_of(event, attr)?`,
which turns an absent attribute into `NotNumeric`. Whether a 50 ms run happens to
sample an untimed load with a stack is chance, which is where the one run in ten
comes from. It is not really load. The fix is one line in R's file.

**21. `scenario_mem::both_huge_page_backends_read_the_same_promotion_counter`.** It
already measures its own drift. What remains is the kernel cross-check,
`assert!((low..=high).contains(&reported), ...)`, where `slack = drift + 4` and
`drift = before.abs_diff(after)` over `/proc/meminfo` read once before and once after
five sessions. A machine-wide level that rises and falls during the session is wider
than its net change. The source is machine-wide and the workload creates none of the
huge pages, so there is no workload property here, only a bracket. The bracket has to
be the excursion (min and max of reads interleaved with the rounds), not the
endpoints.

**22. `scenario_mem::compaction_is_seen_...`.** Fixed in the eighth wave: "it treated
a kcompactd wake as proof that compaction ran". It is listed for completeness.

**23. `collect::mem::wss::tests::a_process_that_holds_much_and_touches_little_reports_the_little`.**
`std::thread::sleep(Duration::from_millis(300))`, then `assert!(working >= 0.5 *
WORKING as f64, ...)` and `resident >= 0.9 * RESIDENT`. Both assume the child was
scheduled and kept its pages. Under load a child descheduled for most of 300 ms walks
its 16 MiB fewer times, reclaim clears referenced bits, and memory pressure takes
resident pages. The `Toucher` already shares a handshake page ("A sleep would be a
guess; this is the fact"). Letting it count walks makes the wait a fact too.

**24. `scenario_gpu::a_resident_kernel_separates_from_an_idle_device_on_occupancy_and_on_power`.**
It failed "with `nvidia-smi` reporting the device at 100 % utilisation and 178 MiB
held by somebody else", so the idle baseline was not idle. `device_to_ourselves()`
now runs before the test, but not after, and the assertions (`idle_tail <= 0.2`,
`busy_watts >= idle_watts * 2.0`) are device-wide figures shared with the compositor
and with any other agent's GPU test. The fix is to re-check exclusivity after both
sessions and skip with the numbers, or to read per-process utilisation.

**25. `cross_io::the_crossing_sessions_run_and_say_what_they_found`.** Its lower bound
is already guarded by `lost == 0`. What remains is `const LEAD_IN: Duration =
Duration::from_millis(1000)` ("Three eBPF program sets load in sequence"). Under load,
reads issued before the last set attached are neither matched nor dropped, so
`matched >= 1980` fails with `lost == 0`. The test also runs its two sessions
concurrently (`std::thread::scope`), both machine-wide over the block layer, so each
is the other's neighbour.

**26. `join_engine::the_resident_size_decomposes_into_live_and_retained_...`.**
"118 644 bytes over the interpreter allowance" (`INTERPRETER = 16 pages`, "sixteen
pages here so that a slower interpreter is not mistaken for a wrong tool"). The record
does not say whether it was `live`, `retained` or the residue. `live` comes from the
preload shim's ring, which drops and counts under a slow reader (finding 47), and the
level rows come from the last 100 ms poll. Either could move by 118 KiB under load.
Logging `losses.dropped` of the three heap requests, and the last reading's offset
against the workload's `ARENA` line, would tell which. If it is drops, carry them as
slack. If it is the poll, it is (b).

**27. `encoders::a_known_allocation_lands_on_the_chrome_trace_at_the_time_it_was_made`.**
`const AFTER_MS: f64 = 300.0;`, with the series split at `*at < AFTER_MS * 1000.0` and
`*at > (AFTER_MS + 100.0) * 1000.0`. The workload is `time.sleep(0.3)` after python
starts, and the offset is from the run's start, so under load the interpreter's
startup pushes the allocation past 400 ms. The recorded failure fits exactly:
"9932800 before, 9932800 after". The step had not happened yet. The workload should
say when it allocated.

---

## The patterns that recur

1. **Counting everything the target took part in, when the workload created only a
   subset** (#1, #2, #3, #6, #9, #16, #17). A switch count that includes preemptions,
   a wakeup count that includes interrupts landing on the target, an instruction count
   that includes kernel mode, a run-queue share of a window, a port on a machine-wide
   target. Each is correct about the machine and wrong about the workload. The load
   adds to the count and never subtracts, which is why these fail on the high side.
   Where the kernel keeps a per-task figure for the same quantity (`schedstat`,
   `nvcsw`/`nivcsw`, `RetransSegs` in a private namespace), the test should compare
   against that figure instead of a window share.

2. **A fixed delay standing in for an event** (#5, #7's `ATTACH_MARGIN`, #11, #23,
   #25, #27, and #14's "the startup is the same in both"). "Long enough to attach",
   "long enough to walk", "300 ms after the run began", "a 200 ms deadline". Every
   one is calibrated on a quiet machine, and attach time under load is measured
   (seconds for fentry). The fix is always the same: the workload or the run
   *announces* the moment (a readiness line, a counter in a shared page, a timestamp
   on `CLOCK_MONOTONIC`), and the test waits for it or splits on it.

3. **A bound that ignores a loss or resolution the tool already reports** (#7's late
   ticks, #10's `short`, #18's counted skips, #12 and #13's rebased schedule, #26
   possibly). The run says it dropped, skipped or ran late, and the assertion demands
   the clean-machine answer anyway. The invariant "losses are counted and surfaced"
   applies to tests too: a counted loss belongs in the slack, and a mark belongs
   exactly on the runs that have losses.

Also present: machine-wide levels bracketed by their endpoints (#21, #24), and three
real tool defects found by tests that looked flaky (#8's float cancellation, #19's
submitter at `block_rq_issue`, #20's `value_of` in `stack`). The last two are the
same kind as finding 48: *attribution or reduction that is right on a quiet machine
by coincidence*.

## A rule a brief could state

> **A scenario asserts only what its own workload creates, and it asserts it against
> a reference that is load-proof.** Concretely, before a scenario lands:
>
> 1. *Filter to the workload.* Count only events whose key is a task, a socket
>    (4-tuple and netns) or a file that the workload made. Where a count over the
>    target is unavoidable, assert its floor from the workload and compare the rest
>    against the kernel's per-task account of the same quantity (`schedstat`,
>    `/proc/<pid>/status`, a counter in a private namespace). Never assert a share of
>    a wall-clock window.
> 2. *No sleeps as synchronisation.* Every "wait until X" is an event that the
>    workload or `omn` announces: a readiness line, a shared-page counter, a
>    `CLOCK_MONOTONIC` stamp printed by the workload. Durations are only for how long
>    something runs, never for when it has started.
> 3. *Slack is the tool's own report.* Every bound carries the run's counted
>    `dropped`, `late_ticks` and skips, plus the reference's resolution (a tick, a
>    `times` quantum, one hold). A constant allowance must name what it measures and
>    where it was measured.
> 4. *Separation, not calibration.* A cost, a latency or a ratio is asserted as two
>    runs differing in one variable and separating, with the separation scaled by
>    what each run actually did (its read count, its cpu time). A number that only a
>    quiet machine reaches belongs in `docs/bench.md`, not in `make check`.
> 5. *Rehearse under load once.* A new scenario runs green alongside `stress-ng
>    --cpu N --io 4 --vm 2` (or the full suite) before it merges. If it fails only
>    there, it is either (1)-(4) or a finding, and it is not a tally entry.
