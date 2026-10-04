# The `request` view: what one request costs

Stream request of the nineteenth wave. Tab 4 of `omn top`. It draws one unit
of work -- an HTTP request, a job, a transaction -- as its life by state and
its share of each resource, from `net.request` and, where the run holds it,
the block requests that carried the request's id.

```console
omn top -f examples/vertical-one-request.yml          # live, the vertical case's server
omn top --replay tests/view_fixtures/vertical-requests --view request
cat docs/view/gallery/request--vertical-requests--a002.ansi   # as the screen looked
```

## What it draws

```
5 requests served by server (pid 2759757) · slowest a001 40.2ms · median 30.1ms · in order of arrival  degraded x2

  req      span  its life by state, to one scale: the slowest is 40.2ms             mostly
  a001   40.2ms  ▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀  lock/sleep 99.8%
▶ a002   10.1ms  ██▓▄▄▄▄▄▄▄▄▄▄▄▄▄                                                   disk 81.8% · on cpu 11.1%
  a003   23.0ms  █▓▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▀▄▄▄                              lock/sleep 87.4% · disk 8.56%
  a004   30.1ms  ░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░                  net in 99.9%
  a005   33.1ms  █░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░░             net out 98.8%
```

Three parts, top to bottom:

1. **Every request**, one row each: its span -- from its first byte reaching
   the server's socket to its last byte leaving it -- as a bar split into the
   six states that tile it, **all to one scale**, so a request's length and
   its make-up read together. `mostly` names the one or two states above a
   twentieth of it. `↑`/`↓` select, `s` sorts slowest first and back.
2. **The selected request's waterfall**, on its own time axis from its first
   byte: the network in before `request__start`, the service between its two
   probes, **each block request it issued where it was on the device**, and
   the network out after `request__done` until the last byte left. The block
   requests are the only marks inside the service whose times are known --
   they are `io.latency` events carrying the request's `req` key, read out of
   the serving thread's storage at the issue.
3. **Where its time went** (each state, its share, the evidence the run's
   decomposition grades it with, the lock's waker, the socket), **its share of
   each resource** (its cpu against everything its process took, its device
   time against the run's, the part of its disk wait the device did not do,
   the network on either side, its longest sleep and who ended it), and on one
   line **what sets it apart from the other requests**: each state against
   the median of the others' same state, the two furthest above it.

`enter` narrows every view to the request (`Focus.request`); `l` opens
`lanes` on its thread.

## On the recorded run

`tests/view_fixtures/vertical-requests` is the vertical case's server serving
five requests, each with a cause injected where the test chose it
(`examples/vertical-one-request.yml`). The view names each cause without being
told it, and its tests hold it to them:

| Request | Injected | `mostly` | What sets it apart |
|---|---|---|---|
| a001 | 40 ms on a lock another thread sleeps under | lock/sleep 99.8 % | lock/sleep +40.1 ms; woken by tid 2759758, the lock's holder |
| a002 | 64 pages read with `O_DIRECT` | disk 81.8 % | disk +8.27 ms; 64 block requests drawn across its service, 24 µs of its wait off the device |
| a003 | 20 ms on the lock, then 32 pages | lock/sleep 87.4 % · disk 8.56 % | lock/sleep +20.1 ms · disk +1.97 ms; the 32 block requests sit at the end of its service, after the lock |
| a004 | the client sends half its request, pauses 30 ms | net in 99.9 % | net in +30.1 ms, all of it before `request__start` |
| a005 | the client reads 128 KiB through a 4 KiB buffer | net out 98.8 % | net out +32.7 ms: 28.5 ms blocked in `send` inside the service, 4.21 ms draining after it |

The waterfall shows two things the table of numbers does not: a003's disk
reads come **after** its lock wait (they are at the right end of its service,
on the device row), and a005's network out is mostly **inside** the service
-- the server was blocked in `send` -- with only the tail after
`request__done`.

The snapshots, one per request, are
`docs/view/gallery/request--vertical-requests--a00{1..5}.{txt,ansi}`, drawn by
selecting each with `↓` (`views::request::tests::each_vertical_request_has_its_snapshot`).
The other fixtures draw `net.request`'s refusal in its own words where the
view would be (`request--http-server`: the shell declares no request), or say
the run did not ask for it (`request--machine`), each with how to get a run
that has requests.

## What it reads, and what it does not draw

- **`net.request`'s raw events** only, plus `io.latency`'s when the run holds
  them. It reads no aggregate, so a live run draws the same way as a
  recording: each request appears when it ends, provisional (`live`) until
  the run does (`a_live_run_draws_the_requests_ended_so_far`).
- **`io.latency` is read, not asked for.** The view's declared need stays
  `net.request` alone: a session that asks for requests asks for block
  latency beside them (the vertical session does), and live `omn top` asks
  `io.latency` for the heatmap anyway. Where the run has none, the disk line
  says so rather than draw an empty device row.
- **The order of states inside the service is not drawn**, because it is not
  recorded: `net.request` adds the time up by state in the kernel and keeps
  the sums. The service bar is therefore the states' **proportions**, said
  under it in words, never a sequence. Only the network before and after it
  and the block requests are placed in time.
- **The evidence column** is the decomposition's (`IDENTITY` for a state read
  off the serving thread, `KEY` for the network rows joined by the socket's
  cookie). A run without a decomposition says the evidence is not graded,
  rather than guess it.
- **The cpu share** is against the process's whole cpu over the run (the
  runner's `lifetime.cpu_ns`), which includes its start-up; on the fixture
  that is 16.9 ms, so the five requests together are a small part of it.
- **The device-time share** is against every block request the run saw,
  carried or not -- the server's own start-up writes included.
- **A network side `net.request` could not close is unknown, not zero.**
  When a response had not left the stack, or the first byte was not seen,
  the collector publishes no `net_tx` (`net_rx`) rather than a short one, and
  the view does not fill it in: the side's state holds only the part inside
  the service (the wait in `send` or `recv`), drawn as a lower bound `≥`; the
  span is `≥` and runs to `request__done` (from `request__start`), said in the
  heading; the waterfall marks the edge with `?` and says "unknown after
  done"; no state gets a share of a span that is only a lower bound, so the
  `%` column and `mostly` give times instead. The line under the summary
  counts such requests. This is the third request of
  `examples/vertical-abandoned.yml`, whose client never reads its 16 KiB
  answer (`an_undrained_response_is_unknown_not_gone_when_done`).
- **Requests `net.request` counted and could put in no event** are counted on
  the line under the summary, read off the collector's notes: requests born
  and never ended (with the ids it named, and how many a later
  `request__start` on the same thread replaced), `request__done` with no
  birth, and requests the filter removed. A request with no end has no total,
  so the list cannot draw it, and the view says how many it does not draw
  (`requests_in_no_event_are_counted_on_screen`). `vertical-abandoned` is the
  run that has them; no recording of it is in `tests/view_fixtures/` yet, so
  the tests rewrite the vertical recording into its shape.
- **"No block request" is a zero only from an `io.latency` that watched the
  whole request and lost nothing** -- the join engine's rule. Otherwise the
  view says "no block request seen" and why (`io.latency lost N, so some may
  be missing`, or it was not watching for all of the request), with
  `io.latency`'s loss badge beside it
  (`no_block_request_under_losses_is_none_seen_with_the_loss`).
- `io.latency` held under two labels (a target's and the system's) is one set
  of block requests: an event both saw is counted once, in the request's
  block requests and in the run's device time.
- A focus on a request the list does not hold -- not in any event of the run,
  or hidden by a pid or tid focus -- is said on the line under the summary,
  with the request drawn instead. "What sets it apart" compares against the
  requests the list shows, under the same focus.
- Trust: request figures take `net.request`'s marks (medium determinism, two
  degradations it states, so a badge and no dimming); disk figures take
  `io.latency`'s (low determinism: dimmed, with `~low det`).

## What it does not show

- Requests without probes. `net.request` follows what the program declares
  and infers nothing from a connection, so a server that fires no
  `request__start`/`request__done` has no requests here -- by design, said on
  screen.
- Bytes on the network. `net.request` gives the network's time on either side
  of a request, not its volume; the `flows` view is where bytes are.
- Which lock. The waker's tid is named; the lock's address and the stack
  that took it are not in `net.request`. `wake` and `lanes` (`l`) are the
  next step.
- Requests from several processes are drawn in one list, scaled together;
  nothing groups them by process yet beyond the focus.

**Verdict**: it is the only screen that names, per request and without being
told, why that request was slow -- the lock and who held it, the disk with
each block request on its own time axis, the client that paused or read
slowly -- and what set it apart from its siblings; it does not show the order
of states inside a request (only their sums are recorded), nor anything for a
program that does not declare its requests.
