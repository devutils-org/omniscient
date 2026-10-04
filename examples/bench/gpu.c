/*
 * The bench's GPU workloads: one program, one mode per workload regime.
 *
 * A GPU tracer's cost depends on the regime more than on anything else: a
 * tracer that does work per kernel costs a launch-bound program a large share
 * of its time and a compute-bound one almost nothing. So each mode pins one
 * regime down, and measures itself:
 *
 *   gpu launches N        N launches of `tiny` (one block of 32 threads),
 *                         synchronised every 1024: bound by the host's launch
 *                         path, the most kernels a second this program makes
 *   gpu compute N ITERS   N launches of `spin`, ITERS dependent additions per
 *                         thread over 68 blocks: bound by the device's
 *                         arithmetic, a few kernels a second
 *   gpu memory N MIB      a memset and a copy in, N grid-stride copies of MIB
 *                         MiB, a copy out: bound by the device's memory
 *
 * Each mode is preceded by BENCH_GPU_WARM_MS (500) of spin kernels, outside
 * every measured window, so the device's clocks are up before the timed
 * section starts.
 *
 * The report (one JSON object, to the file BENCH_REPORT names, never to
 * stdout, which is `omn`'s under `omn trace`) carries what the program did and
 * three times on its own CLOCK_MONOTONIC:
 *
 *   init_ns     cuInit to the module loaded: where the CUDA driver loads an
 *               injection library (CUDA_INJECTION64_PATH) and where that
 *               library arms its tracer -- a fixed cost, paid once a run
 *   elapsed_ns  the first launch to the last synchronise: the timed section,
 *               where a tracer's per-kernel cost lands
 *   exit_ns     the end of main to the last exit handler: where a tracer
 *               flushes and ships what it still holds. The handler that
 *               measures it is registered before cuInit, so it runs after
 *               every handler registered there, a tracer's included
 *
 * and section_cpu_ns, the process's cpu over the timed section, every thread
 * of it: the driver's, and a tracer's flush thread.
 *
 * The device is reached through `libcuda.so.1`, linked by name rather than
 * opened at runtime, so that a tracer interposing on the dynamic linker
 * (`gpu.api`'s preload shim) sees the calls; the kernels are PTX in this file,
 * so no toolkit is needed to build or run it.
 *
 * Exit code 2 with a reason on stderr when the machine has no device the
 * driver will open: the bench reads that as "this machine cannot serve me".
 */
#define _GNU_SOURCE
#include <errno.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

/* The driver API, declared here rather than taken from a toolkit header. */
typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;
typedef void *CUmodule;
typedef void *CUfunction;
typedef void *CUstream;
typedef unsigned long long CUdeviceptr;

extern CUresult cuInit(unsigned int flags);
extern CUresult cuDeviceGet(CUdevice *device, int ordinal);
extern CUresult cuCtxCreate_v2(CUcontext *context, unsigned int flags, CUdevice device);
extern CUresult cuCtxSynchronize(void);
extern CUresult cuCtxDestroy_v2(CUcontext context);
extern CUresult cuModuleLoadData(CUmodule *module, const void *image);
extern CUresult cuModuleGetFunction(CUfunction *function, CUmodule module, const char *name);
extern CUresult cuMemAlloc_v2(CUdeviceptr *pointer, size_t bytes);
extern CUresult cuMemcpyHtoD_v2(CUdeviceptr destination, const void *source, size_t bytes);
extern CUresult cuMemcpyDtoH_v2(void *destination, CUdeviceptr source, size_t bytes);
extern CUresult cuMemsetD8_v2(CUdeviceptr destination, unsigned char value, size_t bytes);
extern CUresult cuLaunchKernel(CUfunction function, unsigned int grid_x, unsigned int grid_y,
                               unsigned int grid_z, unsigned int block_x, unsigned int block_y,
                               unsigned int block_z, unsigned int shared_bytes, CUstream stream,
                               void **params, void **extra);

/* `tiny` does almost nothing, so a launch of it costs the device a few
 * microseconds and the host's launch path is the bound. `spin` is a dependent
 * chain of additions whose sum is stored, so `ptxas` keeps the loop. `copy`
 * moves n eight-byte words with a grid-stride loop. The same kernels as
 * tests/gpu_workload.py's, so a cost here is a cost on the kernels the
 * scenarios hold the tracer to. */
static const char PTX[] =
    ".version 6.0\n"
    ".target sm_50\n"
    ".address_size 64\n"
    ".visible .entry tiny(.param .u64 p_out)\n"
    "{\n"
    "  .reg .u64 %rd<4>;\n"
    "  .reg .u32 %r<2>;\n"
    "  ld.param.u64 %rd1, [p_out];\n"
    "  mov.u32 %r1, %tid.x;\n"
    "  cvt.u64.u32 %rd2, %r1;\n"
    "  shl.b64 %rd2, %rd2, 2;\n"
    "  add.u64 %rd3, %rd1, %rd2;\n"
    "  st.global.u32 [%rd3], %r1;\n"
    "  ret;\n"
    "}\n"
    ".visible .entry spin(.param .u64 p_out, .param .u64 p_iters)\n"
    "{\n"
    "  .reg .u64 %rd<8>;\n"
    "  .reg .pred %p1;\n"
    "  ld.param.u64 %rd1, [p_out];\n"
    "  ld.param.u64 %rd2, [p_iters];\n"
    "  mov.u64 %rd3, 0;\n"
    "  mov.u64 %rd4, 0;\n"
    "LOOP:\n"
    "  add.u64 %rd4, %rd4, %rd3;\n"
    "  add.u64 %rd3, %rd3, 1;\n"
    "  setp.lt.u64 %p1, %rd3, %rd2;\n"
    "  @%p1 bra LOOP;\n"
    "  st.global.u64 [%rd1], %rd4;\n"
    "  ret;\n"
    "}\n"
    ".visible .entry copy(.param .u64 p_dst, .param .u64 p_src, .param .u64 p_n)\n"
    "{\n"
    "  .reg .u64 %rd<12>;\n"
    "  .reg .u32 %r<8>;\n"
    "  .reg .pred %p1;\n"
    "  ld.param.u64 %rd1, [p_dst];\n"
    "  ld.param.u64 %rd2, [p_src];\n"
    "  ld.param.u64 %rd3, [p_n];\n"
    "  mov.u32 %r1, %ctaid.x;\n"
    "  mov.u32 %r2, %ntid.x;\n"
    "  mov.u32 %r3, %tid.x;\n"
    "  mov.u32 %r4, %nctaid.x;\n"
    "  mad.lo.u32 %r5, %r1, %r2, %r3;\n"
    "  mul.lo.u32 %r6, %r4, %r2;\n"
    "  cvt.u64.u32 %rd4, %r5;\n"
    "  cvt.u64.u32 %rd5, %r6;\n"
    "LOOP:\n"
    "  setp.ge.u64 %p1, %rd4, %rd3;\n"
    "  @%p1 bra DONE;\n"
    "  shl.b64 %rd6, %rd4, 3;\n"
    "  add.u64 %rd7, %rd2, %rd6;\n"
    "  ld.global.u64 %rd8, [%rd7];\n"
    "  add.u64 %rd9, %rd1, %rd6;\n"
    "  st.global.u64 [%rd9], %rd8;\n"
    "  add.u64 %rd4, %rd4, %rd5;\n"
    "  bra LOOP;\n"
    "DONE:\n"
    "  ret;\n"
    "}\n";

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

static uint64_t cpu_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* A refusal by the driver at setup is the machine's ("no device here"); one
 * after it is this program's, and says which call. */
static void check(const char *call, CUresult code) {
    if (code != 0) {
        fprintf(stderr, "gpu: %s refused with CUDA error %d\n", call, code);
        exit(2);
    }
}

static void pin(void) {
    const char *list = getenv("BENCH_CPUS");
    if (!list || !*list)
        return;
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(atoi(list), &set);
    sched_setaffinity(0, sizeof(set), &set);
}

/* What the exit handler writes: everything but exit_ns is known by then. */
static struct {
    const char *mode;
    uint64_t kernels, memcpy, memset;
    uint64_t init_ns, elapsed_ns, section_cpu_ns;
    uint64_t main_end_ns;
} done;

static void write_report(void) {
    if (!done.main_end_ns)
        return;
    uint64_t exit_ns = now_ns() - done.main_end_ns;
    const char *path = getenv("BENCH_REPORT");
    FILE *out = path ? fopen(path, "w") : stderr;
    if (!out) {
        fprintf(stderr, "gpu: BENCH_REPORT: %s\n", strerror(errno));
        _exit(2);
    }
    fprintf(out,
            "{\"mode\":\"%s\",\"pid\":%d,\"kernels\":%llu,\"memcpy\":%llu,\"memset\":%llu,"
            "\"init_ns\":%llu,\"elapsed_ns\":%llu,\"section_cpu_ns\":%llu,\"exit_ns\":%llu,"
            "\"cpu_ns\":%llu}\n",
            done.mode, getpid(), (unsigned long long)done.kernels,
            (unsigned long long)done.memcpy, (unsigned long long)done.memset,
            (unsigned long long)done.init_ns, (unsigned long long)done.elapsed_ns,
            (unsigned long long)done.section_cpu_ns, (unsigned long long)exit_ns,
            (unsigned long long)cpu_ns());
    fclose(out);
}

static CUmodule module;

static CUfunction function(const char *name) {
    CUfunction found;
    check("cuModuleGetFunction", cuModuleGetFunction(&found, module, name));
    return found;
}

static void launch(CUfunction kernel, unsigned grid, unsigned block, void **params) {
    check("cuLaunchKernel", cuLaunchKernel(kernel, grid, 1, 1, block, 1, 1, 0, NULL, params, NULL));
}

static void launches(uint64_t count) {
    CUfunction tiny = function("tiny");
    CUdeviceptr out;
    check("cuMemAlloc", cuMemAlloc_v2(&out, 4 * 32));
    void *params[] = {&out};
    uint64_t start = now_ns(), cpu = cpu_ns();
    for (uint64_t i = 0; i < count; i++) {
        launch(tiny, 1, 32, params);
        if (i % 1024 == 1023)
            check("cuCtxSynchronize", cuCtxSynchronize());
    }
    check("cuCtxSynchronize", cuCtxSynchronize());
    done.section_cpu_ns = cpu_ns() - cpu;
    done.elapsed_ns = now_ns() - start;
    done.kernels = count;
}

static void compute(uint64_t count, uint64_t iterations) {
    CUfunction spin = function("spin");
    CUdeviceptr out;
    check("cuMemAlloc", cuMemAlloc_v2(&out, 8));
    void *params[] = {&out, &iterations};
    uint64_t start = now_ns(), cpu = cpu_ns();
    for (uint64_t i = 0; i < count; i++)
        launch(spin, 68, 256, params);
    check("cuCtxSynchronize", cuCtxSynchronize());
    done.section_cpu_ns = cpu_ns() - cpu;
    done.elapsed_ns = now_ns() - start;
    done.kernels = count;
}

static void memory(uint64_t count, uint64_t mib) {
    CUfunction copy = function("copy");
    size_t size = (size_t)mib << 20;
    CUdeviceptr source, destination;
    check("cuMemAlloc", cuMemAlloc_v2(&source, size));
    check("cuMemAlloc", cuMemAlloc_v2(&destination, size));
    char *host = malloc(size);
    if (!host) {
        fprintf(stderr, "gpu: cannot allocate %llu MiB on the host\n", (unsigned long long)mib);
        exit(2);
    }
    memset(host, 7, size);
    uint64_t words = size / 8;
    void *params[] = {&destination, &source, &words};
    uint64_t start = now_ns(), cpu = cpu_ns();
    check("cuMemsetD8", cuMemsetD8_v2(source, 7, size));
    check("cuMemcpyHtoD", cuMemcpyHtoD_v2(source, host, size));
    for (uint64_t i = 0; i < count; i++)
        launch(copy, 272, 256, params);
    check("cuCtxSynchronize", cuCtxSynchronize());
    check("cuMemcpyDtoH", cuMemcpyDtoH_v2(host, destination, size));
    done.section_cpu_ns = cpu_ns() - cpu;
    done.elapsed_ns = now_ns() - start;
    done.kernels = count;
    done.memcpy = 2;
    done.memset = 1;
    free(host);
}

/* Spin kernels until BENCH_GPU_WARM_MS (500 by default) have passed, before
 * the timed section. The clocks of a GPU that is not locked follow its load:
 * a device idle for a second runs the first kernels after it at a fraction of
 * its clock, and a run started two seconds later -- behind a slower tracer's
 * startup -- would be slowed by the idle, not by the tracer. Measured: the
 * same `compute` section took 78 ms warm and 119 ms after an idle second,
 * and 200 ms of warming after three idle seconds still left it at 100 ms;
 * 500 ms brought it to 79-80 ms. */
static void warm(void) {
    const char *setting = getenv("BENCH_GPU_WARM_MS");
    uint64_t ms = setting ? strtoull(setting, NULL, 10) : 500;
    CUfunction spin = function("spin");
    CUdeviceptr out;
    check("cuMemAlloc", cuMemAlloc_v2(&out, 8));
    uint64_t iterations = 20000;
    void *params[] = {&out, &iterations};
    uint64_t until = now_ns() + ms * 1000000ull;
    while (now_ns() < until) {
        for (int i = 0; i < 8; i++)
            launch(spin, 68, 256, params);
        check("cuCtxSynchronize", cuCtxSynchronize());
    }
}

static void usage(void) {
    fprintf(stderr, "usage: gpu launches N | gpu compute N ITERS | gpu memory N MIB\n");
    exit(64);
}

int main(int argc, char **argv) {
    if (argc < 3)
        usage();
    pin();
    done.mode = argv[1];
    /* Before cuInit: the handlers registered there -- a tracer's -- run
     * first, and this one measures them. */
    atexit(write_report);

    uint64_t start = now_ns();
    CUdevice device;
    CUcontext context;
    check("cuInit", cuInit(0));
    check("cuDeviceGet", cuDeviceGet(&device, 0));
    check("cuCtxCreate", cuCtxCreate_v2(&context, 0, device));
    check("cuModuleLoadData", cuModuleLoadData(&module, PTX));
    done.init_ns = now_ns() - start;

    warm();
    uint64_t first = strtoull(argv[2], NULL, 10);
    if (!strcmp(argv[1], "launches") && argc == 3)
        launches(first);
    else if (!strcmp(argv[1], "compute") && argc == 4)
        compute(first, strtoull(argv[3], NULL, 10));
    else if (!strcmp(argv[1], "memory") && argc == 4)
        memory(first, strtoull(argv[3], NULL, 10));
    else
        usage();
    /* The context is left to the driver's own exit, as most programs leave
     * it: destroying it here would move a tracer's last flush out of the
     * exit window this program measures. */
    done.main_end_ns = now_ns();
    return 0;
}
