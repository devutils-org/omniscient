# The correlation keys

`ROADMAP.md` phase 1 asks for a one-page note that justifies each key by the
boundary it crosses. This is that note. The set is closed:
`request::KeyKind` names the kinds and `event::KeyValue` carries the values.
A join rule matches on keys and on nothing else, so the join engine never
needs to know what a `bio` or a `malloc` is.

Three rules govern how keys are compared, and they come before the list:

- **A join compares a key's identity**, `join::identity`, which is the
  `Display` of `KeyValue` with two exceptions. `blk` drops its open timestamp,
  which `scoped_eq` reads separately as the scope (`join::scope`): although
  printed, it is compared only as a scope, never as part of the identity. And
  `iface` is compared by the kernel's index (`iface#<index>`,
  `KeyValue::iface_identity`), by its name only when no index was known at the
  read, and never by a sub-series written after a `/` in the name. `Display`
  keeps the name, because the name is what a person reads. A reading's own
  series -- the baseline `between` differences against -- is still every
  field of the key, so two series of one interface never take each other's
  previous value.
- **Match kinds follow the key.** `mem` is matched by interval, and `blk` by
  scoped equality (`is_scoped`). `wakeup` and `woke` are refused on any key
  but `task`, which they follow from waker to wakee. Every other key is matched by plain equality.
- **An event's first key is its series.** The keys after it are for joins.
  `mem.faults` is keyed `task` first, so a thread's faults are one series, and
  `mem` second, so an interval join can ask which allocation holds the
  address.

| Key | Written | Crosses | Survives | Does not survive |
|---|---|---|---|---|
| `req` | `req:<id>` | user program to kernel, through the serving task | any hook that runs in that task | a hand-off to another thread; writeback |
| `task` | `task:<tid>` | every mechanism: procfs, perf, eBPF, the shim | cpu migration, blocking, exec | exit, then reuse; work done by another task; a pid namespace |
| `proc` | `proc:<pidns>+<pid>` | containers: a pid namespace and the pid inside it | exec; the namespace's whole life | exit, then reuse; `setns` into another namespace |
| `file` | `file:<dev>+<ino>` | descriptors, processes, the page cache | renames, hard links, reopening | btrfs subvolumes; inode reuse; the block layer |
| `sock` | `sock:<cookie>` | softirq processing, `sock_diag` dumps | the socket's whole life | the peer; `accept`; leaving the machine |
| `blk` | `blk:<ptr>@<open_ts>` | insert, issue and completion, across cpus and IRQ | pointer recycling after completion | reuse inside one plug; bio-based stacked devices |
| `mem` | `mem:[<base>,<end>)` | allocator (user) to page fault (kernel) | address reuse between a free and the next alloc | a second address space; what a fault fills beyond its address |
| `gpu` | `gpu:<fence_ctx>+<seqno>` | submitting task, scheduler thread, completion | fence pointer recycling | another fence context (never attached here) |
| `cgroup` | `cgroup:<id>` | cgroup2 files, a container runtime, kernel programs | tasks moving in and out | deletion and re-creation |
| `iface` | `iface:<name>`, compared as `iface#<index>` | driver, qdisc, stack, `af_packet`, netlink, `/proc/net/dev` | a rename | deletion and re-creation; another network namespace |
| `dev` | `dev:<major>+<minor>` | diskstats, the block layer, writeback | a device node's name changing | partition against whole disk; btrfs's anonymous device |
| `cpu` | `cpu:<n>` | per-core counters, idle and frequency, IRQs | the whole run | a task (it does not follow one) |
| `node` | `node:<n>` | where an allocation landed | the whole run | a page migrated later |

## Each key

**`req`, a propagated request identity.** The kernel never sees a request. It
sees a thread receive bytes, take a lock, read a file and send bytes. So the
identity is written, not inferred. A server fires the USDT probe
`request__start(id)`, a program stores the id in the serving task's
`BPF_MAP_TYPE_TASK_STORAGE` slot (`src/bpf/request.rs`), and any program that
later runs *in that task* reads it back. The block layer's issue is such a
hook for a read, which is why `io.latency`'s events carry `req:<id>`
(`src/collect/io/block.rs`), and why a block row is `IDENTITY` rather than
`KEY`. `net.request` produces the key too (`src/collect/net/request.rs`). The
id does not survive a hand-off to another thread unless the server declares
it there. It does not survive a write that leaves through writeback in a
kworker, or kernel work done outside the serving task, such as the receive
softirq before the task reads.

**`task`, a thread.** The most common key: 67 catalog entries and some forty
collectors (`src/collect/**`). It is how every mechanism names whose event it
was, so it is what an `eq` between two families usually uses, and the only
key the `wakeup` chain can follow. The eBPF programs read the kernel's own id
(the initial pid namespace). The procfs sources use the id their target was
named by. The two agree when `omn` runs in the initial pid namespace, which is
the only place it is tested. The key survives migration between cpus,
blocking, syscalls, and exec. An exec that replaces a multithreaded process
gives the exec'ing thread the leader's id, which is the kernel's rule. It does
not survive exit, because the kernel reuses ids. And it does not follow work a
task caused but another task did: writeback, a kworker, a softirq that lands
on whatever was running. **Processes are keyed `task:<pid>` too**
(`src/collect/proc/status.rs`, `src/collect/mem/footprint.rs`), so a process's
series and its main thread's carry the same key.

**`proc`, a process across containers.** The pid namespace a process lives
in, by the inode `/proc/<pid>/ns/pid` names, and its pid inside that
namespace, the last `NSpid` of `/proc/<pid>/status`
(`KeyValue::proc_of`). Two containers' pid 1 are two keys, and a process is
never its main thread, which `task:<pid>` is. Kept rather than removed because
`CATALOG.md` ("The container trap") names it as the answer to `--cgroup` not
being a pid filter. `KeyValue::proc_of` is the one way to make it, and **no
collector calls it yet**: the per-process sources (`src/collect/proc/status.rs`,
`src/collect/mem/footprint.rs`) are to carry it as a second key after `task`,
and their catalog entries to list it (`docs/open-findings.md`, the sixteenth
wave, stream engine). Until then `task` stands in, with the namespace limit
given above. Reading the namespace link takes the access `ptrace` read does,
so a process of another user has no `proc` key, and says so by having none.

**`file`, device and inode.** From `inode->i_sb->s_dev` and `i_ino`, read at
the VFS by `io.vfs`, `io.fsync` and `io.pagecache` (`src/collect/io/`). It
names a file whatever descriptor, process or name reached it, so it survives
renames, hard links and reopening. It does not survive btrfs: the superblock
is one device for the whole filesystem, while `stat` gives each subvolume its
own, and each subvolume numbers its inodes from 256 (`src/bpf/io_vfs.rs`). It
does not survive an inode number reused after a delete. And it does not reach
the block layer, where a request carries no file.

**`sock`, the socket cookie.** Made by `bpf_get_socket_cookie`, or read from
`sock_diag` for `net.tcp.rtt` (`src/collect/net/*.rs`). The kernel assigns it
once per socket and does not hand it out again. It is the key that survives
softirq processing: a packet received in interrupt context, with no owning
task current, still names its socket. The cookie is one end of a connection,
so the peer has its own cookie even on loopback. A listening socket and the
socket `accept` returns are two cookies. And it means nothing off the machine.

**`blk`, a block request, scoped.** The `struct request` pointer plus the
kernel's own `request.start_time_ns`, the only key two collectors that never
met can agree on (`src/collect/io/block.rs`, the only producer). It crosses
insert, issue and completion across cpus and into IRQ context. It is scoped
because the kernel recycles a request within milliseconds: `STATE.md` shows
one pointer under two open timestamps four milliseconds apart. What it does
not survive: `start_time_ns` comes from a clock cached for the length of a
plug, so two lives of one pointer inside one plug share the whole key. The
engine pairs them by order, counts them in `states.scope_collapsed`, and
rates the match `TEMPORAL` within the key (`DECISIONS.md`, the thirteenth
wave). A request without `start_time_ns` is scoped by the run's first sight of
it, and says so. A bio-based stacked device (device-mapper) has no request of
its own; its I/O is keyed at the device underneath.

**`mem`, an address range.** Matched by interval. The preload shim writes each
allocation's `[ptr, ptr+size)` and a one-byte range at `ptr` for its free
(`src/collect/mem/heap/alloc.rs`). `mem.faults` (`src/collect/mem/page_faults.rs`)
writes the faulting address as a one-byte range, and `mem.ibs` the sampled
address, or its cache line for the sharing question (`src/collect/mem/ibs.rs`). It
crosses from a user-space allocator to the kernel's memory manager, which
share nothing but addresses. An address reused after a free is a new state,
because the free closes the old one. The key carries **no address space**:
two processes' identical addresses are one range to it, which is sound only
because both sides of a session's interval join watch one workload. And a
fault's address is not the extent the fault filled. Under THP `always` one
fault fills an aligned 2 MiB page (`DECISIONS.md`, "What the measurement
taught").

**`gpu`, a fence.** `fence_ctx` plus `seqno`, from the DRM scheduler's job
tracepoints (`src/collect/gpu/job.rs`). A pair rather than a pointer, because
fence pointers are recycled and a context's sequence numbers are not. It
crosses the submitting task, the scheduler's kernel thread, and the
completion. A sequence number means nothing in another context, and each ring
has its own. **It has never attached here**: the development machine's NVIDIA
driver has no DRM scheduler.

**`cgroup`, a control group.** The cgroup directory's inode number, which on
cgroup2 is also the id a kernel program reads for a task's cgroup
(`src/collect/cpu/cgroup.rs` takes it from the directory's `stat`). It crosses from what a person or a container runtime names
(`--cgroup`, `--container`) to what the kernel compares. It is the group's,
not a task's, so it survives tasks moving in and out. It does not survive the
group's deletion: a re-created group of the same path has a new id.

**`iface`, a network interface.** Carries the kernel's index and the name,
prints the name, and is compared by the index (`iface#<index>`). A producer
that reads only a name -- `net.bytes` (`src/collect/net/dev.rs`) and
`net.iface` (`src/collect/net/iface.rs`) read `/proc/net/dev` -- has the index
filled from the name at the read (`KeyValue::iface`, `if_nametoindex`), while
the interface exists. So a rename mid-run is one interface, and `net.iface`'s
`lo/drops`, which carries its counter kind after a `/` for want of a
sub-series, is `lo`: no Linux interface name holds a `/`, so everything before
it is the interface. A key whose index stayed 0 -- the interface was gone at
the read, or lives in another network namespace -- is compared by that name.
Held by `join_engine::two_sources_of_one_interface_join_by_it_and_credit_what_lo_moved`:
`net.iface` and `net.bytes` of `lo` joined by `between`, credited at least
what `/sys/class/net/lo/statistics` counted across the test's own burst and at
most what it counted across the run; before, the two names matched nothing.
A producer that carries the kernel's index (`net.qdisc`, `net.capture`,
`net.hwts`) meets a named one by it: held by
`join_engine::a_queued_packet_on_lo_joins_lo_s_link_counters_by_its_index`,
`net.qdisc` against `net.iface`, which skips wherever lo queues nothing --
lo is `noqueue` by default -- and so runs only on a machine whose lo was given
a discipline that queues (`netem`).

**The index is per network namespace**, and the identity does not say which.
The tool resolves a name in its own namespace, but `net.qdisc` reads the
packets of every namespace, and lo is 1 in each: measured, 2000 packets queued
on the lo of an unprivileged namespace were all credited to this namespace's
`iface:lo`. Until the kernel-side producers carry the namespace beside the
index (filed, `docs/open-findings.md`, the sixteenth wave, stream engine), a
join of a machine-wide interface source with a named one is sound only on a
machine with one network namespace, or with the kernel-side source targeted at
an interface that exists only in this one. A deleted and re-created interface
has a new index, so it is a new key.

**`dev`, a block device.** The major and minor as `/proc/diskstats` writes
them (`src/collect/io/diskstats.rs`), as `request.part` holds them in the
block layer (`src/collect/io/block.rs`), and as the flushed superblock's
`s_dev` for `io.writeback`. The pair rather than the name, because a name is a
symbolic link away from meaning something else. A partition and its whole disk
are two keys: the block layer names the partition. On btrfs `io.writeback`'s
key is the filesystem's anonymous device, which matches no diskstats line.

**`cpu`, a logical cpu.** Per-core counters, `/proc/stat`'s per-cpu lines,
frequency, idle states, throttling, IRQs, softirqs and run-queue latency
(`src/collect/{cpu,power,sched,net}/`). It exists because a mean across 24
cores describes no core. It is where something happened, not whose, so it
does not follow a task that migrates. SMT siblings are two cpus.

**`node`, a memory node.** `mem.numa` alone (`src/collect/mem/numa.rs`),
because a local and a remote allocation differ only by the node they landed
on. It says where a page was when it was read, not where it will be after the
kernel's NUMA balancing moves it.

## What is not a key

- **Machine-wide state has no key.** An `overlap` rule over it reads its
  `from` source as one series keyed `machine` (`join::MACHINE`). That is a
  rule's convention, not a `KeyKind`, and it is `TEMPORAL` by construction.
- **The counter kind of `net.iface`** is carried in the key's name, after a
  `/`, for want of a sub-series. A join no longer sees it (the identity drops
  it); a series still does. `docs/open-requests.md` has the request for a
  sub-series (line 3054).
