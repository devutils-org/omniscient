#!/usr/bin/env python3
"""The lanes view's workload: threads and GPU kernels in a known schedule.

Three named threads take turns: `lane-a`, then `lane-b`, then `lane-c`, for
`rounds` rounds, each holding the cpu for `turn_s` per turn while the others
are blocked waiting for the baton. Then `lane-gpu` launches `spin` kernels one
at a time on the device and waits for each, blocked in the driver. The
schedule is in `manifest.json`; the device part needs `libcuda.so.1` and is
skipped, with a note, where there is none.
"""

import ctypes
import os
import sys
import threading

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, ".."))
sys.path.insert(0, os.path.join(HERE, "../../../tests"))
sys.dont_write_bytecode = True
import common  # noqa: E402

TRUTH = common.manifest(__file__)
CU_CTX_SCHED_BLOCKING_SYNC = 4


def lane(index, batons):
    names = TRUTH["lanes"]
    common.name(names[index])
    mine, following = batons[index], batons[(index + 1) % len(names)]
    for _ in range(TRUTH["rounds"]):
        mine.wait()
        mine.clear()
        common.burn(TRUTH["turn_s"])
        following.set()


def device():
    """A context whose synchronisation blocks the thread, or `None`."""
    try:
        import gpu_workload as gw
    except ImportError as error:
        common.note(f"lanes: no device part: {error}")
        return None, None
    try:
        lib = ctypes.CDLL("libcuda.so.1")
    except OSError as error:
        common.note(f"lanes: no device part: libcuda.so.1 is not loadable here: {error}")
        return None, None

    class Blocking(gw.Cuda):
        def __init__(self):
            self.lib = lib
            self.lib.cuLaunchKernel.argtypes = (
                [ctypes.c_void_p]
                + [ctypes.c_uint] * 7
                + [ctypes.c_void_p, ctypes.POINTER(ctypes.c_void_p), ctypes.c_void_p]
            )
            self.context = ctypes.c_void_p()
            handle = ctypes.c_int()
            self.call("cuInit", self.lib.cuInit(0))
            self.call("cuDeviceGet", self.lib.cuDeviceGet(ctypes.byref(handle), 0))
            self.call(
                "cuCtxCreate",
                self.lib.cuCtxCreate_v2(
                    ctypes.byref(self.context), CU_CTX_SCHED_BLOCKING_SYNC, handle
                ),
            )

    try:
        return gw, Blocking()
    except SystemExit as error:
        common.note(f"lanes: no device part: {error}")
        return None, None


def gpu_lane():
    plan = TRUTH["gpu"]
    common.name(plan["thread"])
    gw, cuda = device()
    if cuda is None:
        return
    spin = cuda.load_spin()
    out = cuda.allocate(8)
    args = gw.params(out, plan["iterations"])
    for _ in range(plan["launches"]):
        cuda.launch(spin, args)
        cuda.synchronize()
    common.note(f"lanes: {plan['launches']} kernels")


def main():
    common.name("lanes-main")
    batons = [threading.Event() for _ in TRUTH["lanes"]]
    threads = [threading.Thread(target=lane, args=(index, batons)) for index in range(len(batons))]
    for each in threads:
        each.start()
    batons[0].set()
    for each in threads:
        each.join()
    gpu = threading.Thread(target=gpu_lane)
    gpu.start()
    gpu.join()
    common.note("lanes: done")


if __name__ == "__main__":
    main()
