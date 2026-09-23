# Using `omn` from the command line

## The model: a measurement is five axes

```
omn  <SOURCE>   <TARGET>        -m <MODE>   -e <EMIT>   [-d duration]   [-- program]
     what       on what         how         in what shape
     io.latency --system        trace       hist
```

- **SOURCE** — what you measure, always `family.metric` (`io.latency`,
  `proc.syscall`, `cpu.profile`…). A program such as `/usr/bin/ls` is not a
  source: it is a target.
- **TARGET** — what to measure it on. `--system` (the whole machine),
  `--pid 1234`, `--cgroup …`, or a program to launch, written **after `--`**.
- **MODE** (`-m`) — `trace` (every event), `count`, `sample@997hz`,
  `poll@100ms`…
- **EMIT** (`-e`) — the shape of the result: `summary`, `hist`, `topk:5`, `rate`,
  `raw`, `timeline`, `stack`.

`omn <source> …` is short for `omn trace <source> …`.

**The rule behind the errors in a row**: the tool only picks for you when the
catalog leaves a single option. `io.latency` accepts two modes and five emits,
so it refuses to guess and asks. That is deliberate: choosing between two
legitimate answers is how a tool reports one number while you read another.

**The shortcut that stops the guessing**: `omn list <source>` shows everything a
source accepts.

```console
./target/debug/omn list io.latency     # modes, emits, targets, keys, and whether it works here
./target/debug/omn list                # all 100 sources, and what this machine can serve
```

## Commands that work

```console
# Disk latency of the whole machine, 2 seconds, as a histogram
./target/debug/omn io.latency --system -m trace -e hist -d 2s

# Read latency of one program (a file on the real disk, cold cache)
./target/debug/omn io.latency -m trace -e summary -- dd if=/home/me/big.bin of=/dev/null bs=1M
#  -> 512 requests, mean 838 us, max 5 ms

# The system calls of ls
./target/debug/omn proc.syscall -m trace -e summary -- /usr/bin/ls

# Where a program spends its cpu, with call stacks
./target/debug/omn cpu.profile -m sample@997hz -e stack -- ./my_program

# Where it is blocked, and on what
./target/debug/omn sched.offcpu -m trace -e stack -- ./my_program
```

## Three traps

1. **`/tmp` is a tmpfs**: there is no disk under it. Reading a file in `/tmp`
   gives 0 `io.latency` events. To measure the disk, use a file on `/home`.
2. **A write has no submitter.** `dd … of=file` on a launched program gives 0
   events, and the tool says why in a `degraded:` line: the write leaves later,
   through a kworker's writeback. To see requests on your program, measure
   **reads**.
3. **`cargo run` can cost you sources.** If the code changed, `cargo run`
   relinks the binary, and a relink drops `CAP_BPF` and `CAP_PERFMON`. The eBPF
   sources (`io.latency`, `sched.*`, `proc.syscall`…) are then refused for lack
   of privilege, with nothing obvious saying so. Run `make` once, then
   `./target/debug/omn`.

## Reading an error

The exit code says **whose** refusal it is:

| Code | Meaning | What to do |
|---|---|---|
| **2** | the request cannot mean anything (wrong mode, emit, target…) | fix the command; the message proposes the alternative |
| **1** | this machine cannot serve it | another machine, or a privilege |
| **3** | the request is valid and this build has not written that collector | nothing to fix on your side |

## Sessions instead of flags

- `--dump-request` at the end of a command prints the equivalent YAML session.
- `omn -f examples/<session>.yml` runs a ready-made one.
- `GUIDE.md` lists every session with the question it answers, which is often
  easier than building the command by hand.
