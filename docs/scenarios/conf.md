# Scenarios — family `conf`

What is worth checking in this family, and how you know the answer is right.

Three sources that read settings rather than measure anything. The failure
this family has to avoid is **a plausible default**: a setting that could not
be read answered with the kernel's documented default, a cgroup limit read at
the wrong level, a value read from the parent before a launched program set
its own. Every check below compares a reading with a second, independent path
to the same value.

The vocabulary is `docs/CATALOG.md` (`conf`), the table
`catalog/settings.yaml`, and the decisions `docs/DECISIONS.md`, "Settled in
the thirty-first wave".

---

## The checks

| Question | The property chosen | Checked by |
|---|---|---|
| Are the machine's settings what sysctl and sysfs say? | `sysctl -n` and a `cat` of the THP and cpu files, read beside | `family_conf::the_machine_settings_are_what_sysctl_and_sysfs_say` |
| Is every cpufreq policy read, and none invented? | the count of `cpufreq/policy*/scaling_governor` | the same test |
| Is a setting that cannot be read said, not guessed? | every row: `set` with a value, or another state with a reason and no value | the same test, and `settings::tests::this_process_reads_its_own_settings_and_no_setting_is_answered_with_a_guess` |
| Does a launched program read with the settings its caller gave it? | `taskset -c 0 nice -n 7 chrt -b 0 prlimit --nofile=1000:2000 sh -c 'echo 345 > oom_score_adj; exec sleep'` | `family_conf::the_process_settings_are_what_the_process_was_given` |
| Are a cgroup's limits the scope's? | `systemd-run --user --scope -p MemoryHigh=200M -p MemoryMax=256M` | `family_conf::the_cgroup_limits_are_the_scope_s` |
| Is a setting changed mid-run seen, and a derived score not? | the program writes its own `oom_score_adj` and narrows its affinity after 300 ms | `family_conf::a_setting_changed_during_the_run_is_said` |

## What was found building it

- **A zombie reads like a process with settings missing.** At the end of a
  launched run the program has exited and is not yet reaped; its `status`
  has no `THP_enabled`, and `conf.changed` first reported `1 -> absent`. A
  process whose `stat` says `Z` or `X` is now gone, and the last reading
  taken while it ran is its end.
- **`oom_score` moves without anyone setting anything.** It is the kernel's
  derived score, and changed between two reads of an idle shell. It is read
  (the victim rule's mechanism names it) and marked `computed`, never
  reported as a change.
- **The kernel's build configuration is `/proc/config.gz` on Arch**, which is
  gzip; there is no `/boot/config-<release>`. It is inflated in process
  (`miniz_oxide`, since 2026-11-12; `zcat` before), once per process, and
  held to what `zcat` reads where the kernel serves one
  (`settings::tests::proc_config_gz_reads_as_zcat_reads_it`). The boot file
  is tried first.
- **A launched program is read after it starts**: the snapshot under
  `taskset -c 0` read `0` on five runs of five, and the one under `sh -c
  'echo 700 > oom_score_adj; exec sleep'` read 700.

## What it does not cover

- The settings the table lists, about seventy, chosen because a source of
  the catalog shows their effect: not `sysctl -a`.
- A per-thread setting is read for the first 64 threads; beyond, the rest
  are counted and not read.
- A change and its undoing between two polls of `conf.changed` is not seen.
