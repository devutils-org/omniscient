# Stream reference, the twenty-fifth wave

Branch `feat/w25-reference`. Done sequentially by one agent, in the main
checkout.

## What landed

1. **`perf-trace-count` skips on perf's lost events** (the runner's owner):
   a parser error starting `common::INCOMPLETE` ("the reference is
   incomplete: ") is a skip quoting the tool, not a failure; `perf trace -s`'s
   `LOST N events!` is one.
2. **`load-latency`'s `why`**: the cause of IBS's delivered rate (66-74 % of
   4000 Hz with `cnt_ctl` and `swfilt`, perf alone 2 640 a second) and of
   `peer` ("core, same node Any cache hit", 3.5 % to 16 % run to run under
   both tools), from mem's twenty-fourth wave.
3. **`cpu.cycles.system` over a known load**: eight spinning interpreters
   as the window, so omn's start and exit (about 0.5e9 cycles) are a fixed
   share of tens of billions rather than of the machine's idle 0.4-0.9e9.
4. **New pairs for what landed**: `nstat-active-opens` (`net.snmp -k
   counter` against `nstat -saz TcpActiveOpens`) and `ss-s-transports`
   (`net.sockstat` against `ss -s`), with the parsers `ss_s_transport` and
   `omn_sockstat`.

5. **More pairs, after the wave's first pass**: `vmstat-in`
   (`sched.interrupts --filter 'kind == "hard"'` against vmstat's `in`:
   19 930 against 20 013 a second; with the softirqs, 17 % over, which is
   why `kind` is now a filter field of `sched.interrupts`), `vmstat-r`
   (`sched.runqueue` against `r`), `ss-tuan-sockets` (`net.sockets`
   against `ss -tuan`: 56 against 56), `top-batch-cpus` (`omn top --batch`'s
   machine row against `top -b`: 0.091 against 0.083), all held here
   unprivileged; and, for the privileged run, `iostat-util` (`io.util`
   against `%util`, the omn side checked on a real capture of a 1 GiB direct
   write, 14.3 %), `pidstat-w-wakeups` (`power.wakeups --value per_second`,
   930/s captured over `switch.py 1000`), `perf-sched-timehist-delay`
   (`sched.slices`' `wake_ns` against `sch delay`, the timehist parser now
   reading that column), and `runqlen` (`sched.runqlen` against
   runqlen.bt, its omn capture awaiting the first privileged run).

## Shown vs reasoned

- Both new pairs held on this machine, unprivileged: TcpActiveOpens omn 65
  against nstat 65; `ss -s` TCP 10, UDP 4, RAW 1 on both sides. The family
  tally: net 8 held, 2 skipped, 0 failed.
- The skip on lost events: unit test (the parser's error carries the marker).
- `cpu.cycles.system`: reasoned; `perf stat -a` needs privileges here.

## Not done

1. **The pairs left of the list**: `io.vfs` against vfsstat or `strace -c`,
   runqlat's mean against `wake_to_run_mean_ns`, `perf lock contention`'s
   type against `topk@kind`, turbostat's `Busy%`/`Avg_MHz`, `energy-core`
   against `perf stat -a`, `ss -ti`'s other fields, tcpaccept, tcplife per
   launch, the drop stacks: each wants a root tool this machine has neither
   installed nor granted, and no capture of it to write a parser against.
2. **The pairs rewritten as `omn ... --system -- workload`** (perfstat 3,
   power 1, io 3, top 6, net): each needs its privileged tool to be shown
   unchanged; not touched blind.
3. **A `mem.slab` pair** against `slabtop -o -s c`: both sides need root
   (`/proc/slabinfo`); wanted once `slabtop` is in the sudoers line.

## Tests

`--test reference` (every family: 172 passed), clippy, fmt.
