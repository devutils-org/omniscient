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
            + [ctypes.c_uint] * 6
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

    def launch(self, function, args):
        self.call(
            "cuLaunchKernel",
            self.lib.cuLaunchKernel(function, 68, 1, 1, 256, 1, 1, 0, None, args, None),
        )

    def synchronize(self):
        self.lib.cuCtxSynchronize()


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


def idle(duration_ms):
    """Touches no GPU at all, for `duration_ms`.

    The baseline every other command is read against, and a scenario of its own:
    a program that never opens a context must be reported as holding zero video
    memory, not as having no measurement.
    """
    time.sleep(duration_ms / 1000.0)


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
    else:
        fail(f"unknown workload `{' '.join(argv[1:])}`")


if __name__ == "__main__":
    main(sys.argv)
