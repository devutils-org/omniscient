# Stream power, the twenty-fifth wave

Branch `feat/w25-power`. Done sequentially by one agent, in the main checkout.

## Outside the owned files, said first

- `CATALOG.md`: the `power.cstate` and `power.wakeups` rows, a
  `power.process_energy` row.

## What landed

1. **`power.cstate -m count`** (`-e rate`): a counted window over cpuidle.
2. **`power.wakeups --system`**: powertop's list, one event per process that
   woke, keyed `task`, `comm` and (where readable) `proc`; the per-process
   events carry `comm` and those keys too.
3. **`power.process_energy`**: a per-process energy estimate, the package's
   counted RAPL energy times the process's share of busy cpu time, labelled
   `estimate` on every event, `package_joules` beside it.

## Shown vs reasoned

- `power.cstate -m count -e rate --system -- sleep 1` (shown): 24 per-cpu
  rates; unit test on a scratch tree (8 ms of residency over the window from
  a baseline at `start`).
- `power.wakeups --system` (shown): a loop of 5 ms sleeps read 194 and 196
  wakeups a second from the machine-wide list; unit test for a process gone,
  one new and one idle.
- `power.process_energy` (reasoned): RAPL is refused here; unit tests on
  `/proc/stat`, `/proc/<pid>/stat` and the share.

## Privileged runs wanted

- `omn trace power.process_energy -m count -e raw -- stress-ng --cpu 4 -t 5s`
  under paranoid 0 (or CAP_PERFMON): `share` near 4 of the busy cpus, `joules`
  that share of `package_joules`; compare with `powertop --time=5 --csv`'s
  per-process power estimate for the same command.

## Not done

Nothing of the stream's list.

## Tests

`--lib collect::power`, `--test family_power --test scenario_power --test
cli --test catalog_model --test lazy`, clippy, fmt.

## Decision lines for `DECISIONS.md`

- A per-process energy number is its own source, `power.process_energy`,
  whose every event says it is an estimate; `power.energy` keeps refusing a
  process target.
