# `flows`: where the bytes went

Stream flows of the nineteenth wave. One of the nine views of `omn top`
(`docs/view/README.md`), in `view/src/views/flows.rs`.

```console
omn top --replay docs/view/gallery/flows-run/data --view flows
cat docs/view/gallery/flows--file-server.ansi      # the snapshot, in colour
```

## What it draws

Two bands, each a Sankey diagram drawn in half-block cells, the height of a
ribbon in proportion to its bytes:

```
 network   thread ──▶ socket ──▶ ┃stack┃ ──▶ interface (the machine's)
 storage   thread ──▶ file   ──▶ ┃cache┃ ──▶ device    (the machine's)
```

- **thread**: each thread of the target that moved a byte, named by the
  command it moved the most bytes under (a thread an `exec` renamed is the
  program it became), its tid under it when it has the room.
- **socket / file**: each socket or file the target moved bytes through, out
  (sent, written; green) above in (received, read; cyan), with a sparkline of
  when its bytes moved, its rate, its calls and the time a call took. Past six,
  the smallest are drawn as one row (`181 more files`). The rate is the
  socket's or the file's over the run; a ribbon from a thread is its bytes,
  drawn by width, with no rate of its own.
- **stack / cache**: the kernel layer between the target's bytes and the
  machine's, drawn as a junction rather than as edges. No source ties one
  socket to one interface, or one file's bytes to one device, so the view
  does not invent the edges: the target's bytes arrive on its left, the
  machine's leave on its right, both on **one scale**, and the gap between
  the two heights is the finding.
- **interface / device**: the machine's, everybody's bytes over the run, dark
  grey, with a sparkline each. Those that moved nothing are counted as idle.

Each band's footer says the two sides and their ratio:
`sockets 112MiB -> interfaces 224MiB = 2.00x`, `files 115MiB -> devices
15.3MiB = 0.13x`. The selected socket or file (`↑`/`↓`) is marked `▶`, and the
two lines under the bands say everything about it: what it is, its bytes each
way, calls and rates, its pid and thread; `enter` narrows every view to that
thread.

The trust style is the frame's: each band's title carries its two sources
with their backends and badges; ribbons and figures are dimmed when their
result must not be read at face value (all four sources are `low`
determinism, so on this machine everything is); a loss is a badge on the
socket or file it struck -- whichever of the band's results its rows came
from, not only the first -- and a sentence in the detail lines when that
object is selected. A source that was
refused shows its reason where its column would be; one this run did not ask
for, or asked in a form that carries no bytes, says so in place, and the other
side of the band is still drawn.

## What it reads

| Column | Source | Emit | What it gives |
|---|---|---|---|
| thread -> socket | `net.socket` | `raw` | one row per thread, op, socket and latency bucket per drain: `bytes`, `count`, `total_ns`, `errors`, `family`, `dport` |
| thread -> file | `io.fs` | `raw` | the same per file (`file:<dev>+<ino>`), with `fs` and the superblock's device |
| thread -> descriptor | `io.syscall` | `raw` | the same per descriptor, when `io.fs` is not in the run: a descriptor may be a file, a pipe or a socket, and is named `fd N` |
| interface | `net.bytes` | `timeline` | bytes per interface per poll, the machine's |
| device | `io.bytes` | `timeline` | bytes per device per poll, the machine's, reads and writes together |

A row is not a call: `count` says how many calls it stands for, and `bytes` is
summed over them in the kernel. The view adds rows; it never multiplies.

**The default `omn top` request does not carry bytes per file.** The table of
needs (`docs/view/README.md`) asks `io.syscall` as `summary,hist` -- latency
per thread, which has no bytes -- and does not ask `io.fs`. So on every common
fixture, and live without a session, the storage band draws the devices and
says in place why its files are missing. A session that asks the rows draws
it whole:

```yaml
requests:
- {source: net.socket, label: net.socket, mode: trace, emit: [raw, hist], target: &t {launch: [./prog]}}
- {source: io.fs, label: io.fs, mode: trace, emit: [raw], target: *t}
- {source: net.bytes, label: net.bytes@system, mode: poll@250ms, emit: [timeline, summary], target: system}
- {source: io.bytes, label: io.bytes@system, mode: poll@250ms, emit: [timeline, summary], target: system}
```

`omn top -f that.yml` -- the recording below is exactly that, with
`io.syscall` beside it. The handoff (`docs/fixes/w19-flows.md`) asks for
`io.syscall`'s `raw` in the default needs.

Live, the rows arrive as the run goes, and the interfaces and devices -- a
timeline, which a live run holds only at its end -- are said to come then.

## The recording it is judged on

`docs/view/gallery/flows-run/`: `record.sh` and the run it wrote (`data/`,
456 KB). The common fixtures carry no bytes per file, so this one is the
flows view's own: a small file server -- CPython's threaded `http.server`
with a durable access log (an `fsync` per request) and uploads written and
synced -- under a client in another process that downloads three files of
8 MiB, 1 MiB and 64 KiB and uploads one of 256 KiB, twelve times, on one
keep-alive connection to loopback. 2.6 s on this machine.

What `flows--file-server` shows, and what the test holds:

| Figure | Drawn | Ground truth | Held by |
|---|---|---|---|
| the three largest files read | 96.0 MiB, 12.0 MiB, 768 KiB | 12 x 8 MiB, 12 x 1 MiB, 12 x 64 KiB, exactly | `each_file_holds_the_bytes_the_workload_moved_through_it` |
| uploads written | in the `181 more files` row | twelve of 262 144 bytes, exactly, on the `io.fs` rows the band sums (the band draws them as part of its "more files" row, so this is held on the recording, and the band total below on the view) | the same |
| band total | 115 MiB | the rows' sum, to the byte, "more files" included | the same |
| the socket | one: 109 MiB sent, 3.00 MiB received | 108.75 MiB of files plus 8.4 KiB of headers sent; 3 MiB of uploads plus 4 KiB of requests received | `the_socket_and_the_interface_disagree_by_the_loopback_double_count` |
| lo / the socket | 2.00x | loopback counts a byte once sent and once received | the same, in [1.9, 2.1] |
| devices / files | 0.13x | the 110 MiB of reads were page-cache hits; 3 MiB of writes plus the log's syncs and btrfs's metadata reached the disk | reasoned, not held: the device's bytes are the machine's |

The storage band also shows what the server's startup cost: the main thread
(`python3`) reading 2.9 MiB of modules on its way up, against the serving
thread's 112 MiB.

## The gallery

| Snapshot | What it shows |
|---|---|
| `flows--file-server` | both bands whole: the recording above |
| `flows--http-server` | the network band whole -- one socket, 28.3 MiB sent, lo at 2.00x -- and the storage band's files said missing (no `raw`), its devices idle |
| `flows--sqlite-commits` | no socket moved a byte; the files said missing, the device's 28.5 MiB drawn |
| `flows--cpython-hot`, `-heavier` | the same shape: a cpu-bound program moves almost nothing |
| `flows--machine` | no target: only the machine's columns, wlan0 and the disk |
| `flows--sqlite-latency`, `flows--vertical-requests` | sessions that asked none of these sources: every column says so |

## What it does not do

- **The file -> device edge is said in words, not drawn as a band.**
  `io.fs` now names the block devices under each file's filesystem
  (`blk_devs`, in the keys `io.bytes` uses), so a device a file of the
  target sits above says `◂ N files` beside it, and the file's detail line
  says `above block device 259:0`. A file over several devices is counted on
  each: how its bytes split between them is not measured. Drawing the edge
  as a Sankey ribbon from the file's row to the device's is the next step.
- **It does not draw the socket -> interface edge**: `net.socket` does not
  carry the interface of the socket's route yet (`net.socket`'s `iface`, a
  collector's field), so the junction stays the honest drawing.
- **It does not name a file**: `io.fs` keys a file by device and inode, and a
  path is not something it records.
- **The machine's side is everybody's.** On a busy machine the interface or
  device column holds other processes' bytes; the ratio says so rather than
  dividing them.
- **Reads and writes are one figure on a device, and rx and tx on an
  interface**: `io.bytes`' and `net.bytes`' series are keyed by device and
  interface only.
- **A view area under 62x15 cells gets a sentence** naming the size it needs
  and the size it has: below that a band cannot hold its heading, two rows
  of diagram and its footer. A band too short for every node drops the
  smallest rows and says so in its footer, in the loss colour, with their
  count and bytes (`181 files (5.41MiB) not drawn` at 80x24), so the drawn
  rows and the footer's total can be reconciled.
- **With nothing to select, it says why per band**: `no socket of the
  target's moved a byte` only when the rows were in the run and carried
  none; `bytes per file were not measured` when they were not.
- **On `io.syscall`'s rows alone it draws descriptors, not files**, and no
  ratio to the devices: a descriptor may be a socket or a pipe, so its bytes
  can be the network band's bytes again and never reach a device. The
  heading says `descriptor`, the footer says `no ratio` and why; held by
  `descriptors_are_not_compared_with_devices`.

**Verdict:** flows is the only view that puts the target's own bytes and the
machine's on one scale across the kernel layer between them, so the page
cache's absorption (0.13x) and loopback's double count (2.00x) are seen as a
change of height rather than computed; it does not say which socket used
which interface or which file's bytes reached which device -- nothing
measures those edges -- and on the default request it has no bytes per file
at all.
