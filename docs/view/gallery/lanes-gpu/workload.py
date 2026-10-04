#!/usr/bin/env python3
"""A host program feeding a GPU, recorded for the lanes view.

Three named threads and two streams, in the shape of a training loop:

- `loader` prepares each batch on the cpu (a Python loop) and hands it over
  through a queue of two;
- `feeder` takes a batch, copies it to the device, launches a long `spin` on
  one stream and a `copy` on another, and waits for both in
  `cuCtxSynchronize` -- blocked, not spinning: the context is made with
  `CU_CTX_SCHED_BLOCKING_SYNC`, so the wait is a block the scheduler sees;
- `logger` wakes every 200 ms and writes a line to standard error.

The first half's batches are dear to prepare and cheap to compute, so the
feeder starves on the queue; the second half's are the reverse, so the loader
blocks on a full queue while the device works. Which thread waits for which,
and when it changes, is the program's choice, decided here.

The kernels and the driver calls are `tests/gpu_workload.py`'s, opened at
runtime through `libcuda.so.1`: no toolkit is needed.
"""

import ctypes
import os
import queue
import sys
import threading
import time

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "../../../../tests"))
sys.dont_write_bytecode = True
import gpu_workload as gw  # noqa: E402

BATCHES = 24
CU_CTX_SCHED_BLOCKING_SYNC = 4
libc = ctypes.CDLL(None)


def name(comm):
    """The thread's name as the kernel shows it (`PR_SET_NAME`)."""
    libc.prctl(15, comm.encode(), 0, 0, 0)


def note(text):
    print(text, file=sys.stderr, flush=True)


class Device(gw.Cuda):
    """`gw.Cuda`, with a context whose synchronisation blocks the thread."""

    def __init__(self):
        self.lib = ctypes.CDLL("libcuda.so.1")
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
            "cuCtxCreate",
            self.lib.cuCtxCreate_v2(
                ctypes.byref(self.context), CU_CTX_SCHED_BLOCKING_SYNC, device
            ),
        )


def prepare(index):
    """A batch's worth of cpu: dearer in the first half."""
    rounds = 950_000 if index < BATCHES // 2 else 120_000
    total = 0
    for value in range(rounds):
        total += value * value % 7
    return total


def loader(batches):
    name("loader")
    for index in range(BATCHES):
        batches.put((index, prepare(index)))
    batches.put(None)


def logger(stop):
    name("logger")
    while not stop.wait(0.2):
        note(f"logger: {time.monotonic():.3f}")


def main():
    name("feeder")
    cuda = Device()
    spin = cuda.load_spin()
    kernels = cuda.load_trace()
    size = 16 << 20
    host = ctypes.create_string_buffer(size)
    source = cuda.allocate(size)
    destination = cuda.allocate(size)
    out = cuda.allocate(8)
    compute, copy = cuda.stream(), cuda.stream()
    copy_args = gw.params(destination, source, size // 8)

    batches = queue.Queue(maxsize=2)
    stop = threading.Event()
    threads = [
        threading.Thread(target=loader, args=(batches,)),
        threading.Thread(target=logger, args=(stop,)),
    ]
    for thread in threads:
        thread.start()
    while True:
        batch = batches.get()
        if batch is None:
            break
        index, _ = batch
        cuda.to_device(source, host, size)
        # Cheap in the first half, dear in the second.
        iterations = gw.SPIN_ITERATIONS // 8 if index < BATCHES // 2 else gw.SPIN_ITERATIONS * 5
        spin_args = gw.params(out, iterations)
        cuda.launch(spin, spin_args, stream=compute)
        cuda.launch(kernels["copy"], copy_args, grid=272, block=256, stream=copy)
        cuda.synchronize()
        cuda.to_host(host, destination, 4096)
    stop.set()
    for thread in threads:
        thread.join()
    note(f"{BATCHES} batches")


if __name__ == "__main__":
    main()
