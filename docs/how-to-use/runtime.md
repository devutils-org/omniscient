# `runtime`: garbage collectors, JITs, runtime allocations, your own probes

There is **no lazy default**: each source answers for one language runtime,
and which one a program runs is not known before it runs. Name the source.

All four read probes the runtime (or your program) publishes, mostly USDT. **A
runtime built without its probes reports no probe**, which is not the same as
a runtime that did not pause: the `python3` of most distributions is built
without them.

## Which source for which question

| Question | Source | Mode | What the runtime must provide |
|---|---|---|---|
| How long are GC pauses? | `runtime.gc` | `trace -e hist` | python `gc__start/gc__done`, node, hotspot `gc__begin/gc__end`, ruby |
| What does the JIT compile, and when? | `runtime.jit` | `trace -e raw` | USDT, or a `/tmp/perf-<pid>.map` (`node --perf-basic-prof`, a JVM agent) |
| How much does the runtime allocate (objects)? | `runtime.alloc` | `trace -e hist` | hotspot `object__alloc` under `-XX:+ExtendedDTraceProbes` |
| A probe of my own, or any USDT marker | `runtime.custom` | `trace` or `count` | the probe, named by provider and probe |

## Examples

```console
omn trace runtime.gc -m trace -e hist,summary -- python3 app.py
omn trace runtime.jit -m trace -e raw -- node --perf-basic-prof app.js
omn trace runtime.custom -m count -e summary --filter "provider == 'libc' && probe == 'setjmp'" -- ./prog
omn trace runtime.custom -m trace -e hist --filter "provider == 'myapp' && probe == 'query__done'" --pid 1234 -d 30s
```

`runtime.custom` refuses a request that names no probe, and the refusal lists
the probes the target has. Arguments come back as `arg0`, `arg1`, ... as the
probe's descriptor declares them.

## Targets

A launch or `--pid` (`runtime.custom` also `--tid`). For a launched program
the probes are found before it execs, in the executable and the libraries its
dynamic section names; a library loaded later with `dlopen` is not among
them.

## Cost

Each probe hit is a uprobe trap, about 740 ns, against about 70 ns for the
preload shim. Fine for GC pauses and JIT events; a runtime allocating a
million objects a second is a different program under `runtime.alloc`, which
is why it is `med`. A `bpf:uprobe` on an arbitrary symbol is `high`: low-rate
call sites only.

## Related

- The C heap under a runtime is `mem.heap.alloc`, not `runtime.alloc`.
- A CPython or JIT profile is `cpu.profile`: it reads the runtime's
  `/tmp/perf-<pid>.map` and names frames `py::<function>:<file>`
  (`python3 -X perf`).
- `net.request` uses the same mechanism for request boundaries a server
  declares.
