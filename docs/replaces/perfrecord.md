# What omn replaces: the `perfrecord` family

`perf record`/`report`, `perf top`, `perf mem` and `perf c2c` against
`cpu.profile`, `mem.latency` and `mem.sharing`. The pairs are in
`catalog/references/perfrecord.yaml`, their parsers (both sides: omn's values
are summed out of its JSON by `omn_parse` parsers, not read at one path) in
`tests/reference/perfrecord.rs`, and the shared runner runs them
(`cargo test --test reference pairs_perfrecord -- --nocapture`). The profile
pairs attach both observers to one run of the workload (`run: attach`);
`perf mem` and `perf c2c` cannot attach to a pid here, so those pairs
alternate separate runs and compare medians. Shares or means are compared,
never counts. Measured on an AMD Ryzen 9 3900X (Zen 2, IBS), kernel 7.2,
`perf` 7.2.8, `perf_event_paranoid` -1, under the load of the twenty-third
wave's other streams (`docs/fixes/w23-perfrecord.md`).

| Tool and use | omn equivalent | Covered | Same numbers | Missing |
|---|---|---|---|---|
| `perf record -F 997 -- prog` + `perf report --no-children --sort dso,sym`: the top functions by self share | `omn trace cpu.profile -m sample@997hz -e stack --format json -- prog`, leaves summed; `omn diff A B`'s "own" table prints them | most: launch, pid, system, cgroup targets; user and kernel frames; names from symbol tables, an address folded into its object where none | yes, within 0.05 abs, one run both see: `hot_beta` 0.48-0.52 vs 0.49-0.51, `hot_alpha` 0.19-0.21 vs 0.19-0.21, `hot_gamma` 0.29-0.31 vs 0.28-0.30 (pair `hot-functions`) | a flat per-function table in `trace`'s own output (`-e topk` keyed by function); sampling on cycles (`pmu` has no collector, `pmu:sw` serves, cpu-clock); a `comm` field on `pmu:sw` samples (`--filter 'comm == ...'` drops them all, counted and named by `filtered.unseen_fields`) |
| `perf report --sort dso,sym` on stripped libraries | the same, an unnamed address keyed by its object (`libcrypto.so.3+0x...`) | yes | yes, within 0.05 abs, one run both see: `libcrypto.so.3` 0.59-0.62 vs 0.58-0.61 (pair `stripped-library`) | per-object sort (`--sort dso`) as an omn key |
| `perf report --children` (inclusive shares), `perf report -g` call graphs | `-e stack`: whole stacks, so inclusive shares are a sum over them; `omn top --view flame` draws them | yes, reasoned | no pair: the self shares above are read from the same stacks | an inclusive share per function in the output |
| `perf top -p PID` | `omn top --view flame --pid PID`, live; the numbers are `cpu.profile`'s | the live view, yes | yes, within 0.05 abs, both attached to one run: `hot_beta` 0.49-0.51 vs 0.47-0.51 (pair `live-top`) | none for the share; `perf top`'s per-instruction annotate (`s`) |
| `perf annotate` | none | no | no pair | instruction-level attribution of samples |
| `perf record -e <any event>` | the catalog's sources only | in part | no pair | sampling on an arbitrary PMU event or tracepoint |
| `perf mem record` + `perf mem report`: the loads' latency in cycles | `omn trace mem.latency -m sample@4000hz -e hist --format json -- prog` | yes, through IBS on AMD (PEBS has no collector yet) | within 10 % on a quiet machine, medians of five separate runs: 272-317 vs 279-337 cycles (pair `load-latency`); under heavy load omn read 10-12 % below `perf mem` three runs in a row and the pair failed, an open finding: omn's IBS sampler tags about half the ops `perf mem` does at the same nominal 4000 Hz, and calls 18 % of the timed loads `peer` where `perf mem` calls 6 % a cache hit, so the two means are over different populations | shares by level (`--sort mem`): `level` is a filter field, not a key, so a breakdown is one run per level |
| `perf c2c record` + `perf c2c report`: the lines with the most HITMs | `omn trace mem.sharing -m sample@4000hz -e topk --format json -- prog` | the ranking of lines, yes | yes, within 0.02 abs, medians of three separate runs: the false-shared line takes 1.00 vs 0.999-1.00 of the HITMs, and both name it `0x4040c0` (pair `false-sharing`, keys `top` and `0x4040c0`) | c2c's per-line detail: the offsets inside the line, the code addresses and pids touching each, the load/store split |
