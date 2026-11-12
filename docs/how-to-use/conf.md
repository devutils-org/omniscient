# `conf`: what the machine and the program are set to, and advice

Lazy default: `omn conf -- PROGRAM` (`conf.machine` and `conf.process`,
each read once). The family only reads: no setting is ever written, by any
command, in any mode (`docs/DECISIONS.md`, boundary 5).

## Which source for which question

| Question | Source | Mode |
|---|---|---|
| What are the sysctls, THP, governors, block queues, IRQ affinity, command line? | `conf.machine` | `snapshot -e raw` |
| What is this program's OOM adjustment, nice, policy, affinity, limits? | `conf.process` | `snapshot -e raw`, on `-- PROGRAM` or `--pid` |
| What are its cgroup's memory, cpu, io and pids limits, and which level binds? | `conf.process` | the same: the `cgroup.*` rows |
| Did a setting change while I was measuring? | `conf.changed` | `poll@250ms -e raw` beside the other requests |
| Which setting is behind what I measured, and how would I check a change? | `omn advise --objective O` | below |

## Examples

```console
omn conf.machine
omn conf.process -- ./server
omn conf.process --pid 4242
omn conf.changed -m poll@100ms -- ./job
omn advise --objective latency --save before.json -- ./job
omn advise --objective latency --against before.json -- ./job
```

## Reading a row

Each event is one setting: `setting`, `scope`, `state`, `value`,
`read_from`. `state` is `set`, or `unset` (a command-line parameter nobody
passed: the kernel's default applies), `absent` (this kernel does not have
it) or `unknown` (it could not be read here, with the `reason`). A setting
with several instances -- a cpufreq policy, a block device, an IRQ, a thread
-- carries `instance`. A cgroup limit carries `binding` and `binding_value`
when an ancestor's limit is the lower one; a resource limit carries `hard`
beside the soft value.

## `omn advise`

`omn advise --objective <latency|tail|throughput|energy> -- PROGRAM` runs the
measurements its rules need (`catalog/advice.yaml`), reads the settings
beside them, and prints every rule that held: what was measured, why it
points at the setting, the setting's current value and where it was read,
what changing it is expected to do to which reading, the command a person
would type and at which scope, and caveats. **It runs none of those
commands.**

The objective is required: the same setting is right for one objective and
wrong for another.

The check is the second half. `--save before.json` keeps the answer; after
the change, the same command with `--against before.json` says, rule by
rule, whether the reading each rule predicted moved the way it said, and
which settings differ between the two runs. A reading that moved while the
rule's own setting did not is not credited to the rule.

`omn latency` and `omn saturate` read the settings too, and print the rules
their own measurements hold under `settings that bear on this`.

The rules today, each with a ground-truth test (`tests/advice_truth.rs`;
`bdi.readahead-off` and `mm.thp-defrag-stall` are judged on what a distroproof
guest brought back, `distroproof-advice.toml`, and `tcp.unsent-backlog` is
proved in a network namespace of the test's own):

| Rule | Fires on | Setting |
|---|---|---|
| `cgroup.memory.swap-denied` | an OOM kill in a cgroup denied swap, on a machine with swap | `memory.swap.max` |
| `oom.victim-by-size` | an OOM kill among several programs, no victim adjusted | `oom_score_adj` |
| `sched.shared-cpu` | run-queue waits above 1 ms, an affinity narrower than the machine | the cpu affinity |
| `io.readahead-refused` | a thousand page-cache misses, one block request each, every bdi reading ahead | the program's readahead hint |
| `bdi.readahead-off` | the same, with a backing device at `read_ahead_kb` 0 | `/sys/class/bdi/*/read_ahead_kb` |
| `mm.thp-defrag-stall` | compaction during the run, `defrag` at always, nothing madvised | `transparent_hugepage/defrag` |
| `tcp.unsent-backlog` | a connection of the program's holding over 1 MiB unsent, no low mark | `net.ipv4.tcp_notsent_lowat` |

What a rule says is in `docs/scenarios/advice.md`, with the numbers its
ground truth measured and what it does not cover.
