#!/usr/bin/env python3
"""A GPU workload with a property chosen in this file rather than observed.

Every command here exists so that a scenario can check the tool against a
number this program decided: the bytes of video memory it holds, the window
during which a kernel is resident on the device, the window during which the
device is left alone.

Nothing is printed on standard output. A launched program inherits the stream
`omn` writes its results to, so a chatty workload corrupts the very file the
scenario is about to parse; the notes go to standard error.

The device is reached through `libcuda.so.1`, opened at runtime. There is no
toolkit, no compiler and no linked SDK: a machine without the NVIDIA driver
fails here with a reason on standard error and exit code 2, which a scenario
reads as "this machine cannot serve me" rather than as a defect.
"""

import ctypes
import json
import os
import sys
import time

# A kernel that is deliberately slow and deliberately not optimisable away. The
# accumulator is stored at the end, which is what keeps `ptxas` from deleting
# the loop -- an empty loop compiles to nothing, the kernel returns instantly,
# and a "busy" workload leaves the device idle.
SPIN_PTX = b"""
.version 6.0
.target sm_50
.address_size 64
.visible .entry spin(.param .u64 p_out, .param .u64 p_iters)
{
  .reg .u64 %rd<8>;
  .reg .pred %p1;
  ld.param.u64 %rd1, [p_out];
  ld.param.u64 %rd2, [p_iters];
  mov.u64 %rd3, 0;
  mov.u64 %rd4, 0;
LOOP:
  add.u64 %rd4, %rd4, %rd3;
  add.u64 %rd3, %rd3, 1;
  setp.lt.u64 %p1, %rd3, %rd2;
  @%p1 bra LOOP;
  st.global.u64 [%rd1], %rd4;
  ret;
}
\0"""

# The kernels `gpu.kernel` is held to, in one module. Each one guarantees a
# property this file chooses, which `docs/families/gpu.md` tabulates:
#
# - `tiny` does almost nothing: a launch of it costs the device a few
#   microseconds, so thousands of them in a row are bound by the launch path,
#   which is what fills CUPTI's buffers fastest.
# - `copy` moves `n` eight-byte words from one buffer to another with a
#   grid-stride loop: memory-bound, its time set by the bytes moved.
# - `selftimed` times itself. Thread 0 of every block reads `%globaltimer`
#   (the device's nanosecond clock) and `%clock64` (its SM's cycle counter)
#   before and after the block's work, and writes them with `%smid`, the SM
#   the block ran on. The earliest start and the latest end over the blocks are
#   the kernel's own span, measured on the device by the kernel itself: the
#   reference a tracer's duration is held to.
#
# The spin loop of `selftimed` is `spin`'s, with its accumulator stored for the
# same reason.
TRACE_PTX = b"""
.version 6.0
.target sm_50
.address_size 64
.visible .entry tiny(.param .u64 p_out)
{
  .reg .u64 %rd<4>;
  .reg .u32 %r<2>;
  ld.param.u64 %rd1, [p_out];
  mov.u32 %r1, %tid.x;
  cvt.u64.u32 %rd2, %r1;
  shl.b64 %rd2, %rd2, 2;
  add.u64 %rd3, %rd1, %rd2;
  st.global.u32 [%rd3], %r1;
  ret;
}
.visible .entry copy(.param .u64 p_dst, .param .u64 p_src, .param .u64 p_n)
{
  .reg .u64 %rd<12>;
  .reg .u32 %r<8>;
  .reg .pred %p1;
  ld.param.u64 %rd1, [p_dst];
  ld.param.u64 %rd2, [p_src];
  ld.param.u64 %rd3, [p_n];
  mov.u32 %r1, %ctaid.x;
  mov.u32 %r2, %ntid.x;
  mov.u32 %r3, %tid.x;
  mov.u32 %r4, %nctaid.x;
  mad.lo.u32 %r5, %r1, %r2, %r3;
  mul.lo.u32 %r6, %r4, %r2;
  cvt.u64.u32 %rd4, %r5;
  cvt.u64.u32 %rd5, %r6;
LOOP:
  setp.ge.u64 %p1, %rd4, %rd3;
  @%p1 bra DONE;
  shl.b64 %rd6, %rd4, 3;
  add.u64 %rd7, %rd2, %rd6;
  ld.global.u64 %rd8, [%rd7];
  add.u64 %rd9, %rd1, %rd6;
  st.global.u64 [%rd9], %rd8;
  add.u64 %rd4, %rd4, %rd5;
  bra LOOP;
DONE:
  ret;
}
.visible .entry selftimed(.param .u64 p_out, .param .u64 p_iters)
{
  .reg .u64 %rd<16>;
  .reg .u32 %r<4>;
  .reg .pred %p<3>;
  ld.param.u64 %rd1, [p_out];
  ld.param.u64 %rd2, [p_iters];
  mov.u64 %rd5, %globaltimer;
  mov.u64 %rd6, %clock64;
  mov.u64 %rd3, 0;
  mov.u64 %rd4, 0;
SPIN:
  add.u64 %rd4, %rd4, %rd3;
  add.u64 %rd3, %rd3, 1;
  setp.lt.u64 %p1, %rd3, %rd2;
  @%p1 bra SPIN;
  bar.sync 0;
  mov.u64 %rd7, %globaltimer;
  mov.u64 %rd8, %clock64;
  mov.u32 %r1, %tid.x;
  setp.ne.u32 %p2, %r1, 0;
  @%p2 bra END;
  mov.u32 %r2, %ctaid.x;
  mov.u32 %r3, %smid;
  cvt.u64.u32 %rd9, %r2;
  mul.lo.u64 %rd9, %rd9, 48;
  add.u64 %rd10, %rd1, %rd9;
  st.global.u64 [%rd10], %rd5;
  st.global.u64 [%rd10+8], %rd7;
  st.global.u64 [%rd10+16], %rd6;
  st.global.u64 [%rd10+24], %rd8;
  cvt.u64.u32 %rd11, %r3;
  st.global.u64 [%rd10+32], %rd11;
  st.global.u64 [%rd10+40], %rd4;
END:
  ret;
}
\0"""

# The blocks of a `selftimed` launch: one per SM of the device this was
# written against, so the kernel covers the device and every block starts at
# once.
SELFTIMED_BLOCKS = 68

# Iterations per thread. Chosen so that one launch occupies the device for
# roughly twenty milliseconds on the hardware this was written against: short
# enough that the window ends when it is asked to, long enough that the launch
# queue never runs dry between two of them.
SPIN_ITERATIONS = 2_000_000


class Cuda:
    """The handful of driver-API entry points these workloads need."""

    def __init__(self):
        try:
            self.lib = ctypes.CDLL("libcuda.so.1")
        except OSError as error:
            fail(f"libcuda.so.1 is not loadable here: {error}")
        self.lib.cuLaunchKernel.argtypes = (
            [ctypes.c_void_p]
            + [ctypes.c_uint] * 7
            + [ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p), ctypes.c_void_p]
        )
        self.context = ctypes.c_void_p()
        device = ctypes.c_int()
        self.call("cuInit", self.lib.cuInit(0))
        self.call("cuDeviceGet", self.lib.cuDeviceGet(ctypes.byref(device), 0))
        self.call(
            "cuCtxCreate", self.lib.cuCtxCreate_v2(ctypes.byref(self.context), 0, device)
        )

    def call(self, name, code):
        """Driver-API calls return a code; an ignored one turns a chosen
        property into a coincidence."""
        if code != 0:
            fail(f"{name} refused with CUDA error {code}")

    def allocate(self, size):
        pointer = ctypes.c_ulonglong()
        self.call(
            "cuMemAlloc", self.lib.cuMemAlloc_v2(ctypes.byref(pointer), ctypes.c_size_t(size))
        )
        return pointer

    def load_spin(self):
        module = ctypes.c_void_p()
        function = ctypes.c_void_p()
        self.call("cuModuleLoadData", self.lib.cuModuleLoadData(ctypes.byref(module), SPIN_PTX))
        self.call(
            "cuModuleGetFunction",
            self.lib.cuModuleGetFunction(ctypes.byref(function), module, b"spin"),
        )
        return function

    def launch(self, function, args, grid=68, block=256, stream=None):
        self.call(
            "cuLaunchKernel",
            self.lib.cuLaunchKernel(function, grid, 1, 1, block, 1, 1, 0, stream, args, None),
        )

    def synchronize(self):
        self.call("cuCtxSynchronize", self.lib.cuCtxSynchronize())

    def load_trace(self):
        """The module of `TRACE_PTX`, as a dictionary of its kernels."""
        module = ctypes.c_void_p()
        self.call(
            "cuModuleLoadData", self.lib.cuModuleLoadData(ctypes.byref(module), TRACE_PTX)
        )
        kernels = {}
        for name in (b"tiny", b"copy", b"selftimed"):
            function = ctypes.c_void_p()
            self.call(
                "cuModuleGetFunction",
                self.lib.cuModuleGetFunction(ctypes.byref(function), module, name),
            )
            kernels[name.decode()] = function
        return kernels

    def allocate_managed(self, size):
        pointer = ctypes.c_ulonglong()
        self.call(
            "cuMemAllocManaged",
            self.lib.cuMemAllocManaged(ctypes.byref(pointer), ctypes.c_size_t(size), 1),
        )
        return pointer

    def to_device(self, device, host, size):
        self.call(
            "cuMemcpyHtoD",
            self.lib.cuMemcpyHtoD_v2(device, host, ctypes.c_size_t(size)),
        )

    def to_host(self, host, device, size):
        self.call(
            "cuMemcpyDtoH",
            self.lib.cuMemcpyDtoH_v2(host, device, ctypes.c_size_t(size)),
        )

    def memset(self, device, value, size):
        self.call(
            "cuMemsetD8", self.lib.cuMemsetD8_v2(device, ctypes.c_ubyte(value), ctypes.c_size_t(size))
        )

    def stream(self):
        handle = ctypes.c_void_p()
        # CU_STREAM_NON_BLOCKING: no implicit synchronisation with the legacy
        # default stream, so kernels on two streams may run at once.
        self.call("cuStreamCreate", self.lib.cuStreamCreate(ctypes.byref(handle), 1))
        return handle


def params(*values):
    """A `kernelParams` array: one pointer per parameter, to its value."""
    holders = [ctypes.c_ulonglong(value.value if hasattr(value, "value") else value)
               for value in values]
    array = (ctypes.c_void_p * len(holders))(
        *[ctypes.cast(ctypes.byref(holder), ctypes.c_void_p) for holder in holders]
    )
    # The holders must outlive the launch; the array keeps them reachable.
    array._holders = holders
    return array


def truth(facts):
    """Writes what this run did to the file `OMN_GPU_TRUTH` names, when it
    names one: the numbers a scenario holds the tool to. Never on standard
    output, which is `omn`'s."""
    path = os.environ.get("OMN_GPU_TRUTH")
    if path:
        with open(path, "w") as out:
            json.dump(facts, out)


def fail(reason):
    print(reason, file=sys.stderr)
    raise SystemExit(2)


def note(text):
    print(text, file=sys.stderr, flush=True)


def arena(size, settle_ms, hold_ms):
    """Holds exactly `size` bytes of video memory, after a window holding none.

    The context itself reserves video memory -- on the hardware this was written
    against, around 150 MiB before a single byte is asked for -- so the property
    a scenario can check is not the level but the **step**: the series is flat,
    rises by `size`, and is flat again. Both plateaus are in this run, so the
    context's own reservation is in both and cancels.
    """
    cuda = Cuda()
    note(f"context open, holding nothing, for {settle_ms} ms")
    time.sleep(settle_ms / 1000.0)
    cuda.allocate(size)
    note(f"holding {size} bytes for {hold_ms} ms")
    time.sleep(hold_ms / 1000.0)


def kernel(duration_ms):
    """Keeps a compute kernel resident on the device for `duration_ms`.

    Launches are queued several deep and synchronised only every eighth one, so
    the device is never waiting for the host between two kernels: the property
    is "something was executing on the GPU for the whole window", which is what
    `gpu.engine_busy` claims to report.
    """
    cuda = Cuda()
    function = cuda.load_spin()
    out = cuda.allocate(8)
    iterations = ctypes.c_ulonglong(SPIN_ITERATIONS)
    args = (ctypes.c_void_p * 2)(
        ctypes.cast(ctypes.byref(out), ctypes.c_void_p),
        ctypes.cast(ctypes.byref(iterations), ctypes.c_void_p),
    )
    # One launch before the clock starts, so that the module's just-in-time
    # compilation is not part of the window it is supposed to fill.
    cuda.launch(function, args)
    cuda.synchronize()

    deadline = time.monotonic() + duration_ms / 1000.0
    launched = 0
    while time.monotonic() < deadline:
        cuda.launch(function, args)
        launched += 1
        if launched % 8 == 0:
            cuda.synchronize()
    cuda.synchronize()
    note(f"{launched} kernels over {duration_ms} ms")


def launches(count):
    """Launch-bound: `count` launches of `tiny`, one block of 32 threads each.

    Guarantees the exact number of kernels, and a rate of launches limited by
    the host rather than by the device: the load that fills a tracer's buffers
    fastest. Synchronised every 1024 launches so the launch queue never
    blocks the host.
    """
    cuda = Cuda()
    kernels = cuda.load_trace()
    out = cuda.allocate(4 * 32)
    args = params(out)
    for index in range(count):
        cuda.launch(kernels["tiny"], args, grid=1, block=32)
        if index % 1024 == 1023:
            cuda.synchronize()
    cuda.synchronize()
    truth({"kernels": {"tiny": count}, "memcpy": 0, "memset": 0})
    note(f"{count} launches of tiny")


def forked(count):
    """`count` launches of `tiny`, then a fork: the child exits at once.

    Guarantees the exact number of kernels, all of them the parent's and all
    of them finished before the fork, and a child that exits through `exit`,
    so every `atexit` handler the parent registered runs in it -- the
    injection library's included. The child does not touch CUDA, which the
    driver does not allow after a fork. The parent waits for it at most
    30 s, and fails if it has not exited by then: a library that hangs a
    forked child fails here, with that reason, rather than hanging the run.
    """
    cuda = Cuda()
    kernels = cuda.load_trace()
    out = cuda.allocate(4 * 32)
    args = params(out)
    for _ in range(count):
        cuda.launch(kernels["tiny"], args, grid=1, block=32)
    cuda.synchronize()
    child = os.fork()
    if child == 0:
        sys.exit(0)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        done, _status = os.waitpid(child, os.WNOHANG)
        if done:
            break
        time.sleep(0.01)
    else:
        os.kill(child, 9)
        os.waitpid(child, 0)
        fail(f"the child forked after {count} kernels had not exited after 30 s")
    truth({"kernels": {"tiny": count}, "memcpy": 0, "memset": 0, "child": child})
    note(f"{count} launches of tiny, then a fork whose child exited")


def compute(count):
    """Compute-bound: `count` launches of `spin`, about 2 ms each here.

    Guarantees the exact number of kernels, and that each one's time is set
    by arithmetic: it reads nothing, and writes one word per thread.
    """
    cuda = Cuda()
    function = cuda.load_spin()
    out = cuda.allocate(8)
    args = params(out, SPIN_ITERATIONS // 10)
    for _ in range(count):
        cuda.launch(function, args)
    cuda.synchronize()
    truth({"kernels": {"spin": count}, "memcpy": 0, "memset": 0})
    note(f"{count} launches of spin")


def memory(count, mib):
    """Memory-bound: `count` launches of `copy` over a buffer of `mib` MiB,
    after one memset and one host-to-device copy of it, and before one copy
    back.

    Guarantees the exact number of kernels, of copies and of memsets, and the
    bytes of each: every copy and memset moves exactly `mib` MiB.
    """
    cuda = Cuda()
    kernels = cuda.load_trace()
    size = mib << 20
    source = cuda.allocate(size)
    destination = cuda.allocate(size)
    host = ctypes.create_string_buffer(size)
    cuda.memset(source, 7, size)
    cuda.to_device(source, host, size)
    args = params(destination, source, size // 8)
    for _ in range(count):
        cuda.launch(kernels["copy"], args, grid=272, block=256)
    cuda.synchronize()
    cuda.to_host(host, destination, size)
    truth({
        "kernels": {"copy": count},
        "memcpy": 2,
        "memcpy_bytes": [size, size],
        "memset": 1,
        "memset_bytes": [size],
    })
    note(f"{count} copies of {mib} MiB")


def managed(count, mib):
    """Managed memory: `count` launches of `copy` within one managed buffer of
    `mib` MiB, the host writing all of it between two launches.

    Guarantees the exact number of kernels, and that the pages migrate both
    ways between every two of them: the host's write pulls them back, the
    next kernel pushes them out again. The migrations are not CUPTI kernel
    activity, and must not be counted as kernels.
    """
    cuda = Cuda()
    kernels = cuda.load_trace()
    size = mib << 20
    buffer = cuda.allocate_managed(size)
    half = size // 2
    args = params(buffer.value + half, buffer, half // 8)
    for index in range(count):
        ctypes.memset(buffer.value, index & 0xFF, size)
        cuda.launch(kernels["copy"], args, grid=272, block=256)
        cuda.synchronize()
    truth({"kernels": {"copy": count}, "memcpy": 0, "memset": 0})
    note(f"{count} copies within {mib} MiB of managed memory")


def streams(count, per_stream):
    """Multi-stream: `count` non-blocking streams, `per_stream` launches of a
    short `spin` on each, issued round-robin.

    Guarantees the exact number of kernels on each stream, and that the
    streams are distinct: a tracer that folds them into one has lost the
    concurrency the program asked for.
    """
    cuda = Cuda()
    function = cuda.load_spin()
    out = cuda.allocate(8)
    args = params(out, SPIN_ITERATIONS // 40)
    handles = [cuda.stream() for _ in range(count)]
    for _ in range(per_stream):
        for handle in handles:
            cuda.launch(function, args, grid=4, block=64, stream=handle)
    cuda.synchronize()
    truth({"kernels": {"spin": count * per_stream}, "streams": count,
           "per_stream": per_stream, "memcpy": 0, "memset": 0})
    note(f"{per_stream} launches on each of {count} streams")


def selftimed(count, iterations):
    """`count` launches of `selftimed`, one at a time, each followed by a
    synchronise and a copy of its timings back.

    Guarantees, for every launch: the kernel's own span on the device, from
    its blocks' `%globaltimer` reads; the SMs it ran on; and the host's
    `CLOCK_MONOTONIC` just before the launch and just after the synchronise,
    a window the kernel's start and end must fall in once converted.
    """
    cuda = Cuda()
    kernels = cuda.load_trace()
    slots = SELFTIMED_BLOCKS * 6
    out = cuda.allocate(8 * slots)
    host = (ctypes.c_ulonglong * slots)()
    args = params(out, iterations)
    # One launch outside the record, so the module's first-launch work is not
    # in the first kernel's window.
    cuda.launch(kernels["selftimed"], args, grid=SELFTIMED_BLOCKS, block=128)
    cuda.synchronize()
    runs = []
    for _ in range(count):
        before = time.monotonic_ns()
        cuda.launch(kernels["selftimed"], args, grid=SELFTIMED_BLOCKS, block=128)
        cuda.synchronize()
        after = time.monotonic_ns()
        cuda.to_host(host, out, 8 * slots)
        starts = [host[block * 6] for block in range(SELFTIMED_BLOCKS)]
        ends = [host[block * 6 + 1] for block in range(SELFTIMED_BLOCKS)]
        cycles = [host[block * 6 + 3] - host[block * 6 + 2] for block in range(SELFTIMED_BLOCKS)]
        sms = {host[block * 6 + 4] for block in range(SELFTIMED_BLOCKS)}
        runs.append({
            "span_ns": max(ends) - min(starts),
            "host_before_ns": before,
            "host_after_ns": after,
            "max_block_cycles": max(cycles),
            "sms": len(sms),
        })
    truth({"kernels": {"selftimed": count + 1}, "memcpy": count, "memset": 0,
           "selftimed": runs})
    note(f"{count} self-timed launches")


def idle(duration_ms):
    """Touches no GPU at all, for `duration_ms`.

    The baseline every other command is read against, and a scenario of its own:
    a program that never opens a context must be reported as holding zero video
    memory, not as having no measurement.
    """
    time.sleep(duration_ms / 1000.0)
    truth({"kernels": {}, "memcpy": 0, "memset": 0})


def main(argv):
    if len(argv) < 2:
        fail(__doc__.splitlines()[0])
    command, args = argv[1], [int(value) for value in argv[2:]]
    if command == "arena" and len(args) == 3:
        arena(*args)
    elif command == "kernel" and len(args) == 1:
        kernel(*args)
    elif command == "idle" and len(args) == 1:
        idle(*args)
    elif command == "launches" and len(args) == 1:
        launches(*args)
    elif command == "forked" and len(args) == 1:
        forked(*args)
    elif command == "compute" and len(args) == 1:
        compute(*args)
    elif command == "memory" and len(args) == 2:
        memory(*args)
    elif command == "managed" and len(args) == 2:
        managed(*args)
    elif command == "streams" and len(args) == 2:
        streams(*args)
    elif command == "selftimed" and len(args) == 2:
        selftimed(*args)
    else:
        fail(f"unknown workload `{' '.join(argv[1:])}`")


if __name__ == "__main__":
    main(sys.argv)
