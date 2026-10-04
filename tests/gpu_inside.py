#!/usr/bin/env python3
"""GPU workloads whose inside is chosen in this file: where a kernel spends
its time, line by line.

`gpu.profile` samples program counters inside kernels; the reference it is
held to is a kernel whose hot loop is known by construction. `hotloop` puts a
long chain of dependent multiply-adds on one source line and nothing worth a
sample anywhere else, and says which line in the truth file
(`OMN_GPU_TRUTH`), so a profile of it has one right answer: that line, first.

The kernel is PTX carrying `.loc` line directives, loaded through the driver
with `CU_JIT_GENERATE_LINE_INFO`, so the module the driver compiles has line
information exactly as a `nvcc -lineinfo` build would -- or, for
`hotloop-bare`, the same PTX without the directives, a module with none.

Everything else -- the driver, the context, the conventions on standard
output and exit codes -- is `gpu_workload.py`'s, imported from beside this
file.
"""

import ctypes
import os
import sys

# Importing the shared file must not leave a `__pycache__` in the checkout.
sys.dont_write_bytecode = True
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import gpu_workload  # noqa: E402
from gpu_workload import Cuda, fail, note, params, truth  # noqa: E402

# The source lines the `.loc` directives name, in a file that does not exist:
# line information is what the module says, not what a disk holds.
SOURCE = "hotloop.cu"
SETUP_LINE = 10
HOT_LINE = 20
LOOP_LINE = 21
STORE_LINE = 30

# Sixteen multiply-adds, each waiting on the one before it: a warp at this
# line is stalled on its own result almost every cycle, and the line holds
# sixteen of the loop's nineteen instructions.
CHAIN = "\n".join("  fma.rn.f32 %f1, %f1, %f2, %f3;" for _ in range(16))


def hotloop_ptx(lines):
    loc = (lambda line: f"  .loc 1 {line} 0") if lines else (lambda line: "")
    header = f'.file 1 "{SOURCE}"' if lines else ""
    return f"""
.version 6.0
.target sm_50
.address_size 64
{header}
.visible .entry hotloop(.param .u64 p_out, .param .u64 p_iters)
{{
  .reg .u64 %rd<8>;
  .reg .f32 %f<4>;
  .reg .pred %p<2>;
{loc(SETUP_LINE)}
  ld.param.u64 %rd1, [p_out];
  ld.param.u64 %rd2, [p_iters];
  mov.f32 %f1, 0f3F800000;
  mov.f32 %f2, 0f3F7FFFF0;
  mov.f32 %f3, 0f3C23D70A;
  mov.u64 %rd3, 0;
HOT:
{loc(HOT_LINE)}
{CHAIN}
{loc(LOOP_LINE)}
  add.u64 %rd3, %rd3, 1;
  setp.lt.u64 %p1, %rd3, %rd2;
  @%p1 bra HOT;
{loc(STORE_LINE)}
  st.global.f32 [%rd1], %f1;
  ret;
}}
\0""".encode()


# `CU_JIT_GENERATE_LINE_INFO`, from `cuda.h`.
CU_JIT_GENERATE_LINE_INFO = 13


def load(cuda, lines):
    """The `hotloop` function of a module compiled from its PTX, with line
    information when `lines`."""
    module = ctypes.c_void_p()
    options = (ctypes.c_int * 1)(CU_JIT_GENERATE_LINE_INFO)
    values = (ctypes.c_void_p * 1)(1 if lines else 0)
    cuda.call(
        "cuModuleLoadDataEx",
        cuda.lib.cuModuleLoadDataEx(
            ctypes.byref(module), hotloop_ptx(lines), 1, options, values
        ),
    )
    function = ctypes.c_void_p()
    cuda.call(
        "cuModuleGetFunction",
        cuda.lib.cuModuleGetFunction(ctypes.byref(function), module, b"hotloop"),
    )
    return function


def hotloop(count, iterations, lines=True):
    """`count` launches of `hotloop`, `iterations` turns of its loop per
    thread, one block per SM-sized slice of the device.

    Guarantees the kernel's name, the number of launches, and where its time
    goes: the line of the dependent chain, written in the truth file with the
    lines that hold nothing worth sampling.
    """
    cuda = Cuda()
    function = load(cuda, lines)
    out = cuda.allocate(4)
    args = params(out, iterations)
    for _ in range(count):
        cuda.launch(function, args, grid=gpu_workload.SELFTIMED_BLOCKS, block=128)
    cuda.synchronize()
    truth({
        "kernels": {"hotloop": count},
        "file": SOURCE if lines else None,
        "hot_line": HOT_LINE if lines else None,
        "loop_line": LOOP_LINE if lines else None,
        "cold_lines": [SETUP_LINE, STORE_LINE] if lines else [],
    })
    note(f"{count} launches of hotloop, {iterations} turns each, "
         f"{'with' if lines else 'without'} line information")


def main(argv):
    if len(argv) < 2:
        fail(__doc__.splitlines()[0])
    command, args = argv[1], [int(value) for value in argv[2:]]
    if command == "hotloop" and len(args) == 2:
        hotloop(*args)
    elif command == "hotloop-bare" and len(args) == 2:
        hotloop(*args, lines=False)
    else:
        # Every other workload is the shared file's.
        gpu_workload.main(argv)


if __name__ == "__main__":
    main(sys.argv)
