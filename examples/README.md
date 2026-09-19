# Examples

Each file is a session: the same object a command line builds, written down. Run
one with `omn -f examples/<file>`, or all of them with `make demo`.

They are grouped by the question they answer rather than by family, because that
is how a measurement gets chosen.

## Where did the memory go?

| File | Question |
|---|---|
| `allocation-cost.yml` | **Which allocation actually cost memory?** The correlation: allocation ranges joined to the page faults inside them. |
| `mem-fragmentation.yml` | How much is the allocator holding that nothing is using? |
| `mem-rss-run.yml` | What is a program's resident footprint over its life? |

## What is this machine doing?

| File | Question |
|---|---|
| `cpu-saturation.yml` | Is the CPU the limit, and is anything being throttled? |
| `cpu-counters.yml` | How much work per cycle, and where does it go? |
| `io-pressure.yml` | Is anything stalled on I/O? |
| `io-device-load.yml` | What is the storage actually doing? |
| `net-health.yml` | Is the network the problem, or is it us? |
| `saturation.toml` | A cheap always-on sweep across every resource. In TOML, because the format is a choice about what writes the file. |

## What is it costing?

| File | Question |
|---|---|
| `power-energy.yml` | What is this machine drawing, in joules? |
| `power-confounders.yml` | Is this run comparable to the last one, or was it throttled? |
| `gpu-device-state.yml` | What is the GPU doing, and at what clock and temperature? |
| `gpu-vram-of-a-process.yml` | How much VRAM does one process hold? |

## What is a program doing?

| File | Question |
|---|---|
| `proc-handles.yml` | Is it leaking file descriptors or threads? |
| `runtime-jit.yml` | What did the runtime compile, and when? |
| `sched-wakeup.yml` | **Who unblocked whom?** Needs `CAP_BPF` and `CAP_PERFMON`. |
| `web-latency.yml` | A latency budget across five layers. Most of it needs backends that are not written yet, so it is a `validate` example: `omn validate -f examples/web-latency.yml`. |

## Reading the outputs

Three things appear in every run and are worth knowing how to read.

**The backend line.** `via fs:proc:smaps_rollup` says which of a source's
backends answered. Two measurements taken through different backends are not
comparable, and `--out` writes the full record beside the numbers.

**The loss line.** `no loss: every read succeeded and no tick ran late` is the
only reading that means the numbers are complete. Anything else names what was
lost and why.

**The caveats.** They are properties of the source, true of every run of it, and
they say what the number does *not* mean. A fault count is not a quantity of
memory; a descriptor count without its limit is not a measurement; RSS and PSS
and USS are three different correct answers.
