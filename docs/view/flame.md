# The `flame` view: a flamegraph in the terminal

Stream flame of the nineteenth wave. Where the target spent its cpu
(`cpu.profile`) and where it waited (`sched.offcpu`), as two icicles side by
side; and, given a second run, where the two differ, by the triage's own
arithmetic. `view/src/views/flame.rs`.

```console
omn top --replay tests/view_fixtures/http-server --view flame
omn top --replay tests/view_fixtures/cpython-hot \
        --against tests/view_fixtures/cpython-hot-heavier --view flame
omn top -- ./prog                         # live, then press 2
```

## What is on the screen

- **Two icicles**, on cpu left (warm colours) and off cpu right (cool), root
  at the top, each box as wide as its share: samples on cpu, time off cpu.
  Callees are ordered heaviest first, so the hot path is the leftmost one and
  the order is the same every time. One function has one colour wherever it
  appears, and two neighbours never share one.
- **Chains that never branch are folded.** A frame whose heaviest callee
  holds all of it to within one cell is drawn in one box with that callee:
  `_start › ⋯9⋯ › libpython3.14+0x1b9172` is one row, not eleven. Each column
  goes as deep as it *branches*, not as deep as its stack -- an 88-frame
  kernel path under a Python interpreter fits a 40-row terminal. The detail
  line says what a box folds.
- **Under each icicle, the heaviest functions on their own** -- where the
  samples end, every path that reaches them summed, which an icicle spreads
  over many boxes -- with a bar each; for a lossy run the bar's shaded tail
  is what the function could be had nothing been lost (`trust::error_bar`).
  Then the source's own caveats, in full: a frameless leaf whose caller is
  lost, names a module does not carry.
- **The line above** names each pane's heaviest own function; the **lines
  below** say the selected box whole: its name, samples and the time they
  stand for, its share of everything and of the zoom, its own weight.
- **A pane with nothing to draw** gives its room to the other and says why in
  a strip: refused (the resolver's words), not in this run, or served and
  empty ("no block was recorded").
- **Trust** is the frame's one style: the icicle is dimmed when its source
  is (`cpu.profile` and `sched.offcpu` are both `low` determinism, so it
  always is), the badges -- `29 late ~low det degraded x7` -- are in the
  pane's title.

### Keys

| Key | What it does |
|---|---|
| `↑` `↓` (`k` `j`) | to the caller's box, to the heaviest callee's |
| `h` `l` (`[` `]`) | to the box left or right on the same row |
| `⏎` | zoom into the selected box: it fills the width |
| `⌫` (`u`) | zoom out, to the nearest caller that shows more |
| `/` | type a name; `⏎` searches, `esc` cancels |
| `*` | search the selected box's name |
| `o` | the other pane takes the keys |
| `m` | the active pane alone, full width |
| `t` | off cpu, one subtree per thread (from the blocks themselves) |
| `d` | in a comparison: absolute, or shapes |
| `esc` | clear the search and the zoom |

A search paints every matching box magenta and says, per pane, how many
frames matched and **the share of the profile they account for**, a match
under a match counted once -- the question a search in a flamegraph is for.

The frame's `f` focus narrows the off-cpu icicle to a thread or a process
when the run kept the blocks themselves (`raw` beside `stack`, as every
`omn top` run does); the on-cpu icicle cannot be, since `cpu.profile`'s stack
emit carries no thread, and its pane says so rather than draw the whole as if
it were the part.

## Two runs: `--against`

`omn top --replay A --against B` draws each icicle as the difference:
**red** where B spent more, **blue** where it spent less, **grey** where the
two are within the noise -- deep red and blue at half again or half, or for a
frame one run lacks. A box is as wide as the mean of the two runs (linear, so
a caller is never narrower than its callees); functions whose own cost moved
beyond the noise are underlined. The line above is the triage's sentence; the
list under the icicle is the movements, largest first, each with its ratio
and its distance from zero in standard deviations.

The arithmetic is `omn diff`'s (`src/triage/diff.rs`), restated in the view
since `omn-view` links nothing of `omn`, and held to the same test (one
function doubled is the only difference; a run against itself has none):

- **absolute, not shares**: two runs at one rate are compared in samples,
  at two rates in time;
- **beyond 3σ of a sample count's Poisson noise**, the variance of a count
  times a scale being the count times the scale squared;
- **the losses of both runs said**, under the panes.

`d` switches to **shapes**: the second run scaled to the first's total, the
same noise rule applied to what is left. The two modes answer different
questions, and the fixtures show why both are needed (below).

**Off cpu has no noise model**: a block's duration is not a count. A box is
coloured when trust's own difference rule says so (outside ±5 %) and the
movement is more than a speck (0.25 % of the larger run); the sentence says
there is no noise model rather than give a σ.

## Live

The runner names stacks when a run ends, so while it goes each pane says how
many samples or blocks have arrived on how many stacks, with a sparkline of
when they arrived (`flame-live--sqlite-commits`: the forty commits, one by
one, as a comb), dimmed `live`. The icicle replaces it when the run ends.

## What the fixtures show

| Snapshot | What to look at |
|---|---|
| `flame--http-server` | off cpu splits in half: one thread in `select` (`libc+0xa08f2`), the handler in `__recv`; on cpu, 57 % of the samples under one interpreter frame and 41 % in the `fstatat` system call -- the listing stats every entry of a 15 000-entry directory -- with `link_path_walk`, `__d_lookup_rcu` heaviest on their own |
| `flame-threads--http-server` | `t` and `m`: the same off-cpu time under one root per thread |
| `flame-zoomed--http-server` | `⏎` on the `stat` path: the kernel's path walk fills the width |
| `flame-search--http-server` | `/lookup`: 16 frames, 20 % of the on-cpu samples; none off cpu |
| `flame--sqlite-commits` | every block is `fdatasync` (100 % own); on cpu, `sqlite3` carries no symbols and is named by offset |
| `flame--sqlite-latency` | 43 samples, and still the btrfs fsync path, folded into a dozen rows |
| `flame--cpython-hot` | a flat interpreter: the time is spread over `libpython` offsets, and half the samples landed in a frameless leaf -- the caveat under it says so |
| `flame-against--cpython-hot--cpython-hot-heavier` | absolute: the heavier run is x2.03, and 38 of 378 functions moved beyond the noise, **every one of them up** -- led by `__tls_get_addr`, +408 own, 12.3σ |
| `flame-against-shape--…` | `d`, shapes: **no function moved** beyond 3σ. The second run is the same program doing twice the work, which neither mode says alone |
| `flame-live--sqlite-commits` | half way through a live run |
| `flame-narrow--http-server` | 80x24 |
| `flame--machine`, `flame--vertical-requests` | runs that did not ask for either source: each pane says so |

## What it does not do

- **It does not see Python functions.** `cases/cpython-interpreter/hot.py`
  is one Python function's time by construction, and the icicle shows the
  interpreter's C frames, unnamed. The triage charges a sample to the frame a
  perf map named (`charged_to`); the view reads no perf map and charges to
  the leaf. A Python-level flamegraph needs either that, or the fixture
  recorded with the interpreter's perf map on.
- **It does not repair stacks.** With `fp` unwinding, a frameless leaf's
  caller is lost, and on `cpython-hot` that is half the samples: the icicle
  is honest about the stacks it was given and says so beneath, but its
  shape below such a leaf is not the call graph.
- **No icicle while a run is live.**
- **A search cannot contain `q`, `?`, a digit, or `f` while a focus is set**:
  the frame takes those keys before a view sees them (filed with core).

**Verdict**: the only place in `omn` that shows a profile's shape --
on-cpu and off-cpu side by side, per thread, and two runs' difference
judged against the noise both absolutely and in shape -- but it draws the
stacks it is given, so an interpreter's own functions and a frameless leaf's
callers stay invisible.
