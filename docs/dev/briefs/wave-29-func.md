# Brief — how long each call of a named function took

The twenty-ninth wave, the item the twenty-eighth left: `omn diff` says which
functions cost cpu and whether that changed, and says too that a share of a
profile is not a duration per call. A campaign that wants "how long does one
`CopyThunk::Execute` take, and does it wait" had no source for it: USDT needs
the program to publish a probe, and `runtime.custom` counts a symbol's hits
without timing them. bcc's `funclatency` answers it with a uprobe on the
function's entry and one on its return; this wave is that, as a source.

Run autonomously on 2026-10-26, in one worktree (`claude/w29`).

## What it built

1. **`runtime.func`** (`catalog/runtime.yaml`), `bpf:uprobe`, trace mode,
   launch, `--pid` and `--tid`. The functions are named by the filter --
   `function == '...'`, `function =~ '...'`, `function in (...)` -- and
   `object == '...'` keeps the search to one file. A name answers as the file
   spells it, as it demangles, or as it demangles without its parameters (the
   C++ name a reader writes, every overload). The objects searched are the
   target's (`collect::runtime::objects`), the functions read from `.symtab`
   and `.dynsym` (`stack::functions_in`). A request naming none is refused; a
   name matching nothing lists the files read and the functions whose names
   contain it; more than 32 functions is refused.
2. **The programs** (`src/bpf/func_latency.rs`): the entry counts one more
   open call for `{thread, function}` and stamps it at that depth; the return
   reads the clock first, closes the call at the thread's depth, and publishes
   start, duration, the return register, thread, depth and comm. Paired per
   thread and per depth, so a recursive function's calls are each timed and
   a return the kernel never ran (a `longjmp`) shifts the depth rather than
   mispairing the next calls. Return probes are attached before entries, so
   no call is stamped that nothing will close. uprobe-multi links on 6.6 and
   later, the uprobe PMU otherwise.
3. **What a run says**: calls timed, the files searched, that the time is
   wall clock (off-cpu included), the calls shorter than 2 us (about 1 us of
   each is the traps), the nested calls (`depth` above 1, inside their
   caller's time), the returns with no entry seen, the calls still open at the
   end, those nested deeper than 64; ring and map losses counted as losses.

## Measured

- The fixture (`tests/scenario_func.rs`, built with `cc`): five 20 ms sleeps
  timed within 1 ms of what the program measured around each call; three
  nested calls of a recursive function at depths 1, 2, 3, the outermost
  within 1 ms of the program's measure; twelve calls from three threads, four
  per thread, on the program's own thread ids; a call still open at the end
  said, not timed; 2000 empty calls flagged as mostly measurement.
- The cost: 200,000 calls of an empty function took 433 ms under the probes,
  about 2 us a call, of which 1.07 us is inside the time measured. At that
  rate the 1 MiB ring loses about 4 % of calls, counted, and the summary
  becomes lower bounds.

## What it leaves

- An aggregated mode (a histogram in the kernel, for functions called
  millions of times a second): the trace is what this wave built, and `-e
  hist` sums it.
- A function inlined everywhere has no entry to probe; a library loaded
  later with `dlopen`, and code no file holds (a JIT's), are not searched.
