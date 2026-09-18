# `runtime` — implementation notes

Owned by the `runtime` thread. Record here what the measurements taught, the
traps that cost time, and anything the family needs from the shared layer.

## Findings

### The JIT symbol map is the one runtime signal that needs no probe

`runtime.jit` reads `/tmp/perf-<pid>.map`, the file a runtime maintains so that
a profiler can name code that exists in no object file. Node writes it with
`--perf-basic-prof`, a JVM with an agent. It is append-only, so the bytes that
appeared since the last read are the code objects created since the last read,
which is a compilation stream without a single probe, a capability or a kernel
version.

Measured on `node --perf-basic-prof` running a recursive fibonacci: 2452 code
objects over a 50 ms run, of which 1839 are V8's own builtins and the rest is
compiled JavaScript.

### A file with no timestamps produces events that say so

The map records that code exists. It does not record when the compiler ran, when
it finished, or when the line was written. So an entry is known to have appeared
between two reads and no more precisely than that, and every event carries that
gap as `ts_err_ns` — the first use in this project of a field the event model
has had since phase 1.

The first read is the harder case. Its entries were in the file before anything
was watching, and for a `--pid` target they may be hours old: the file records
nothing that would bound it. They carry `u32::MAX`, the largest error the model
can express, and an attribute that marks them, rather than the small error that
would be true for a launched program and a lie for an attached one.

Timing a compilation from this file is not possible, and the backend says so as
a degradation rather than subtracting two line arrivals and calling the
difference a duration. That number would be the interval between two code
objects being registered, which is not how long either took to compile.

### A kind is only a kind when the runtime says it is

V8 writes `JS:~name file:line`, `Builtin:Name`, `RegExp:pattern`: the kind of
code comes before the first colon, and separating builtins from compiled
JavaScript is most of what makes the output readable. Reading "everything before
the first colon" as the kind also turns a JVM agent's
`com.example.Service::handle` into a kind called `com.example.service`, which is
a taxonomy this tool invented. Only the prefixes V8 is known to write are read
as kinds; anything else is `code`.

### A map that is not there yet has lost nothing

Every poll before the runtime creates its map finds no file, and the first
version of this collector counted each one as a failed read. A node run then
reported a loss on every invocation, for a file that appeared five milliseconds
later with all of its contents intact: nothing was lost, and a loss line that
cries wolf on every run is how a project teaches its users to ignore the loss
line.

The distinction that matters is whether the map was ever read. Polls that found
nothing before the first successful read are counted and held: if the map is
eventually read, they were harmless; if it never is, they are reported as failed
reads with the reason, because a run that watched a program for a second and saw
nothing must say that it saw nothing — not that nothing was compiled. A map that
existed and then disappeared is a loss immediately, because entries written
after the last read are gone.

## Decisions taken in the catalog

### `runtime.alloc` loses its `preload` backend

The brief suggested implementing `runtime.alloc` through the existing
`LD_PRELOAD` shim, and it would have produced a source that duplicates
`mem.heap.alloc` under a second name. The shim sees `malloc`; a managed runtime
allocates objects out of a heap it maps for itself. V8 never calls `malloc` for
the JavaScript heap, so a `runtime.alloc` built on the shim would have reported
Node's internal C allocations and called them the runtime's allocation activity.

The chain is now the USDT backend alone, with a caveat pointing at
`mem.heap.alloc` for the C allocator beneath the runtime. `runtime.alloc` is
Wave 2 work, and reporting it as out of reach is the truthful state.

### `runtime.jit`'s file backend requires `/tmp`, not the map

The backend was declared as requiring `/tmp/perf-[pid].map`. That is a fact about
how the observed program was started, not about this machine, and a requirement
that is probed against the tool's own pid before a program has even been
launched answers a question nobody asked. It would also refuse every launch
target: the map cannot exist before the program does.

What the machine must provide is `/tmp`, which is probed and verified. Whether
the map appears is a fact about the run, and the run reports it — as data, in
the loss line, with the reason.

## Needs from the shared layer

### A filter is reported as applied and is not applied

This is a correctness bug, and it is in the output rather than in a corner.

```console
$ omn runtime.jit --mode trace --emit summary --filter "kind == 'js'" \
      --format json -- node --perf-basic-prof -e '...'
  ... "count": 2452        # every event, including the 1839 builtins
```

The resolved record printed beside those numbers says:

```json
"filter": {
  "placement": "userspace",
  "reason": "`fs:jit-map` returns whole records, so `runtime.jit` is filtered
             after reading: every event costs its full price before being discarded"
}
```

Nothing discards them. `src/filter.rs` parses and validates the expression,
`src/resolve.rs` decides where it runs and says so, and no stage between the
collector and `emit::aggregate` ever evaluates it. Every family with
`filter_fields` is affected, which is all of them.

What it needs: the run applies the userspace part of the filter to each
collection before aggregating, and counts what it discarded so the output says
how much of the stream the filter removed.

### A requirement can depend on the target rather than on the machine

`Capabilities::probe` takes a requirement and an optional pid. Several
requirements of this family are facts about the program being observed: whether
its binary carries `.note.stapsdt` markers (`usdt:gc`, `usdt:jit`, `usdt:alloc`),
whether it publishes a symbol map. For a launch target the program's path is in
the request and those could be checked before anything is attached — the USDT
markers by reading the ELF notes, which needs no privileges at all.

What it needs: `probe(requirement, &Target)` rather than `probe(requirement,
Option<i32>)`, so a requirement can look at the binary the request names. This
family would then answer "this node was not built with its probes" at resolve
time, by name, instead of after a run that saw nothing.

### `runtime.custom` needs somewhere to put the probe

The brief asks this thread to decide how a user declares a probe and how the
tool refuses one it cannot attach. The design, for the coordinator to place in
the request model:

```yaml
requests:
  - source: runtime.custom
    probe: {kind: usdt, object: /usr/bin/node, provider: node, name: gc__start}
  - source: runtime.custom
    probe: {kind: uprobe, object: /usr/lib/libssl.so.3, symbol: SSL_read}
```

On the command line, one flag: `--probe usdt:/usr/bin/node:node:gc__start` or
`--probe uprobe:/usr/lib/libssl.so.3:SSL_read`.

Three rules make it refuse rather than approximate:

1. A `runtime.custom` request without a probe is refused. It is the one source
   whose meaning the catalog cannot carry, so an unnamed one has no meaning.
2. The object must exist and be readable, and the marker or symbol must be in
   it. Both are checkable without privileges — ELF notes for USDT, the symbol
   table for a uprobe — and the refusal should list what the object does carry,
   the way an unknown source suggests the closest one.
3. A uprobe costs one to two microseconds per hit. A request that puts one on a
   call site the resolver has no rate estimate for cannot be budgeted, so the
   caveat has to travel with the request: on a hot path this is not an
   observation, it is a different program.

The mechanism itself is Wave 2: attaching either kind needs the eBPF layer.

### The emits this family declares are mostly not built yet

`topk` is what makes a JIT stream readable — which methods, how much code — and
`hist` is what makes GC pauses readable. Neither exists (BRIEF G5), so the
example ships with `rate` and `summary` and says why.

## What is still blocked, and on what

| Sources | Blocked on |
|---|---|
| `gc`, `alloc`, `custom` | the eBPF layer and USDT attachment — BRIEF G10, Wave 2 |
| `jit` deoptimizations and compilation timing | the same: the map carries neither |
| `custom` declaration surface | a `probe` field in the request model (above) |
| readable output for all four | `topk` and `hist` — BRIEF G5 |
