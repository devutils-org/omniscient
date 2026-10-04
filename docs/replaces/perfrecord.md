# What omn replaces: the `perfrecord` family

`perf record`/`report`, `perf top`, `perf mem` and `perf c2c` against
`cpu.profile`, `mem.latency` and `mem.sharing`. The pairs are in
`catalog/references/perfrecord.yaml`, their parsers (both sides: omn's values
are summed out of its JSON, not read at one path) in
`tests/reference/perfrecord.rs`. Every pair runs omn and the tool on
alternate runs of the same workload, and compares shares or means, never
counts. Measured on an AMD Ryzen 9 3900X (Zen 2, IBS), kernel 7.2,
`perf_event_paranoid` -1, under the load of the wave's other streams; every
pair passed twice in a row (`docs/fixes/w22-perfrecord.md`).

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `perf record -F 997 -- prog` + `perf report --no-children --sort dso,sym`: the top functions by self share | `omn trace cpu.profile -m sample@997hz -e stack --format json -- prog`, leaves summed; `omn diff A B`'s "own" table prints them | most: launch, pid, system, cgroup targets; user and kernel frames; names from symbol tables, an address folded into its object where none | yes, within 0.05 abs: `hot_beta` 0.50 vs 0.50, `hot_alpha` 0.19-0.20 vs 0.20 (pairs `cpu.profile`/`perf record` keyed `hot_beta`, `hot_alpha`) | a flat per-function table in `trace`'s own output (`-e topk` keyed by function); sampling on cycles (`pmu` has no collector, `pmu:sw` serves, cpu-clock); a `comm` field on `pmu:sw` samples (`--filter 'comm == ...'` drops them all, counted and named by `filtered.unseen_fields`) |
| `perf report --sort dso,sym` on stripped libraries | the same, an unnamed address keyed by its object (`libcrypto.so.3+0x...`) | yes | yes, within 0.05 abs: `libcrypto.so.3` 0.596 vs 0.596 (pair keyed `libcrypto.so.3`) | per-object sort (`--sort dso`) as an omn key |
| `perf report --children` (inclusive shares), `perf report -g` call graphs | `-e stack`: whole stacks, so inclusive shares are a sum over them; `omn top --view flame` draws them | yes, reasoned | no pair: the self shares above are read from the same stacks | an inclusive share per function in the output |
| `perf top -p PID` | `omn top --view flame --pid PID`, live; the numbers are `cpu.profile`'s | the live view, yes | yes, within 0.05 abs: `hot_beta` 0.50 vs 0.50 (pair `cpu.profile`/`perf top`) | none for the share; `perf top`'s per-instruction annotate (`s`) |
| `perf annotate` | none | no | no pair | instruction-level attribution of samples |
| `perf record -e <any event>` | the catalog's sources only | in part | no pair | sampling on an arbitrary PMU event or tracepoint |
| `perf mem record` + `perf mem report`: the loads' latency in cycles | `omn trace mem.latency -m sample@4000hz -e hist --format json -- prog` | yes, through IBS on AMD (PEBS has no collector yet) | yes, within 10 %: mean 262-277 vs 268-272 cycles (pair `mem.latency`/`perf mem`) | shares by level (`--sort mem`): `level` is a filter field, not a key, so a breakdown is one run per level |
| `perf c2c record` + `perf c2c report`: the lines with the most HITMs | `omn trace mem.sharing -m sample@4000hz -e topk --format json -- prog` | the ranking of lines, yes | yes, within 0.02 abs: the false-shared line takes 1.00 vs 1.00 of the HITMs (pair `mem.sharing`/`perf c2c` keyed `top`), and both name it `0x4040c0` (the same pair keyed `0x4040c0`) | c2c's per-line detail: the offsets inside the line, the code addresses and pids touching each, the load/store split |
