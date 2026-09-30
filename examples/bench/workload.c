/*
 * The bench's reference workloads, one program with a mode per workload.
 *
 * Every mode measures itself and says what it did, so the bench has something
 * to check the tool against that the tool did not compute: how many operations
 * ran, how long they took on the workload's own CLOCK_MONOTONIC (the tool's
 * clock domain too), and, for the resolution sweep, the moment each marker
 * event was about to happen.
 *
 * The report is one JSON object, written to the file named by BENCH_REPORT
 * rather than to stdout: under `omn trace` the program's stdout is the tool's
 * stdout, and a report mixed into the results is a report nobody can parse.
 *
 * BENCH_CPUS pins the program -- a comma-separated list, the first cpu for a
 * single-threaded mode, one per thread in turn for a threaded one. Affinity is
 * one of the roadmap's environment controls, and it is the one a program can
 * set for itself without privileges.
 *
 *   workload launch                       exit at once
 *   workload spawn K                      fork and exec `launch` K times in turn
 *   workload compute ITERATIONS           integer arithmetic, no memory traffic
 *   workload memory MIB STEPS             a dependent pointer chase over MIB MiB
 *   workload syscall N                    N getppid() calls
 *   workload io-prepare PATH MIB          write the file the io mode reads
 *   workload io PATH                      drop it from the page cache, read it
 *   workload contended THREADS N          THREADS threads, one mutex, N each
 *   workload async THREADS N              THREADS threads, N getppid() each
 *   workload async-malloc THREADS N       THREADS threads, N malloc/free each
 *   workload async-serial THREADS N       async, with a global lock on purpose
 *   workload pairs-signal GAP_NS COUNT    SIGUSR1 twice, GAP_NS apart
 *   workload pairs-malloc GAP_NS COUNT    two marked allocations, GAP_NS apart
 *   workload pairs-fault GAP_NS COUNT     two first touches, GAP_NS apart
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

/* The two allocation sizes the malloc pairs are recognised by. Odd, just
 * under a page, and nothing the C library allocates for itself on the way. */
#define MARK_FIRST 4091
#define MARK_SECOND 4093

static volatile uint64_t sink;

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

static void die(const char *what) {
    fprintf(stderr, "workload: %s: %s\n", what, strerror(errno));
    exit(2);
}

static int cpus[256];
static int ncpus;

static void parse_cpus(void) {
    const char *list = getenv("BENCH_CPUS");
    if (!list || !*list)
        return;
    char *copy = strdup(list), *save = NULL;
    for (char *item = strtok_r(copy, ",", &save); item && ncpus < 256;
         item = strtok_r(NULL, ",", &save))
        cpus[ncpus++] = atoi(item);
    free(copy);
}

static void pin(pthread_t thread, int index) {
    if (ncpus == 0)
        return;
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpus[index % ncpus], &set);
    pthread_setaffinity_np(thread, sizeof(set), &set);
}

#include <sys/wait.h>

/* ------------------------------------------------------------ the report */

static FILE *report;

static void report_open(const char *mode) {
    const char *path = getenv("BENCH_REPORT");
    report = path ? fopen(path, "w") : stdout;
    if (!report)
        die("BENCH_REPORT");
    fprintf(report, "{\"mode\":\"%s\",\"pid\":%d", mode, getpid());
}

static void report_u64(const char *name, uint64_t value) {
    fprintf(report, ",\"%s\":%llu", name, (unsigned long long)value);
}

static void report_close(void) {
    struct rusage usage;
    getrusage(RUSAGE_SELF, &usage);
    report_u64("maxrss_kb", (uint64_t)usage.ru_maxrss);
    /* The kernel's count of the times this process was preempted: what a
     * test that means to preempt it checks that it did. */
    report_u64("involuntary", (uint64_t)usage.ru_nivcsw);
    report_u64("cpu_ns", cpu_ns());
    struct timespec res;
    clock_getres(CLOCK_MONOTONIC, &res);
    report_u64("clock_res_ns", (uint64_t)res.tv_nsec);
    fputs("}\n", report);
    fclose(report);
}

/* --------------------------------------------------------- single thread */

static void compute(uint64_t iterations) {
    uint64_t x = 88172645463325252ull;
    uint64_t start = now_ns();
    for (uint64_t i = 0; i < iterations; i++) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
    }
    uint64_t elapsed = now_ns() - start;
    sink = x;
    report_u64("elapsed_ns", elapsed);
    report_u64("ops", iterations);
}

/* A single random cycle through the buffer, so every load depends on the one
 * before and the prefetchers have no stride to follow. */
static void memory(uint64_t mib, uint64_t steps) {
    size_t slots = (size_t)(mib << 20) / sizeof(size_t);
    size_t bytes = slots * sizeof(size_t);
    size_t *next = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (next == MAP_FAILED)
        die("mmap");
    /* THP is the workload's to choose for its own buffer, and it chooses
     * explicitly: under `always`, whether a 2 MiB page was free when the
     * buffer was touched decides the TLB miss rate, and that differed from
     * one run to the next by more than the gate allows. */
    const char *thp = getenv("BENCH_THP");
    madvise(next, bytes, thp && !strcmp(thp, "huge") ? MADV_HUGEPAGE : MADV_NOHUGEPAGE);
    for (size_t i = 0; i < slots; i++)
        next[i] = i;
    uint64_t seed = 0x9e3779b97f4a7c15ull;
    for (size_t i = slots - 1; i > 0; i--) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        size_t j = seed % i; /* Sattolo: one cycle, never a fixed point */
        size_t t = next[i];
        next[i] = next[j];
        next[j] = t;
    }
    size_t at = 0;
    uint64_t start = now_ns();
    for (uint64_t i = 0; i < steps; i++)
        at = next[at];
    uint64_t elapsed = now_ns() - start;
    sink = at;
    report_u64("elapsed_ns", elapsed);
    report_u64("ops", steps);
    report_u64("bytes", mib << 20);
}

static void syscalls(uint64_t n) {
    uint64_t start = now_ns();
    for (uint64_t i = 0; i < n; i++)
        sink += (uint64_t)syscall(SYS_getppid);
    uint64_t elapsed = now_ns() - start;
    report_u64("elapsed_ns", elapsed);
    report_u64("ops", n);
    report_u64("syscall_nr", SYS_getppid);
}

/* Launch-bound: K processes started and reaped one after another. One launch
 * is under a millisecond and varied by 12 % run to run here, which no gate
 * can pass; K of them in one sample average that out, and K is also an exact
 * count of execs for a source that watches them. */
static void spawn(uint64_t k) {
    char *argv[] = {"workload", "launch", NULL};
    /* BENCH_SPAWN_PATH names what the children exec, so a source watching
     * execs can pick exactly these out of a shared machine: `/proc/self/exe`
     * is the default, and other programs here exec it too. */
    const char *path = getenv("BENCH_SPAWN_PATH");
    if (!path || !*path)
        path = "/proc/self/exe";
    uint64_t start = now_ns();
    for (uint64_t i = 0; i < k; i++) {
        pid_t child = fork();
        if (child < 0)
            die("fork");
        if (child == 0) {
            setenv("BENCH_REPORT", "/dev/null", 1);
            execv(path, argv);
            _exit(127);
        }
        int status;
        if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))
            die("a launched child failed");
    }
    uint64_t elapsed = now_ns() - start;
    report_u64("elapsed_ns", elapsed);
    report_u64("ops", k);
}

static void io_prepare(const char *path, uint64_t mib) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0)
        die(path);
    static char block[1 << 20];
    for (size_t i = 0; i < sizeof(block); i++)
        block[i] = (char)(i * 131);
    for (uint64_t i = 0; i < mib; i++)
        if (write(fd, block, sizeof(block)) != (ssize_t)sizeof(block))
            die("write");
    if (fsync(fd) != 0)
        die("fsync");
    close(fd);
    report_u64("bytes", mib << 20);
}

/* A cold read without root: the pages are clean after io-prepare's fsync, so
 * POSIX_FADV_DONTNEED drops them, which `drop_caches` would do for the whole
 * machine and only root may ask. What is left in the cache afterwards is
 * counted with mincore rather than assumed. */
static void io(const char *path) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        die(path);
    struct stat st;
    if (fstat(fd, &st) != 0)
        die("fstat");
    posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);

    uint64_t resident = 0, pages = 0;
    void *map = mmap(NULL, (size_t)st.st_size, PROT_READ, MAP_SHARED, fd, 0);
    if (map != MAP_FAILED) {
        long page = sysconf(_SC_PAGESIZE);
        pages = ((uint64_t)st.st_size + (uint64_t)page - 1) / (uint64_t)page;
        unsigned char *vec = malloc(pages);
        if (vec && mincore(map, (size_t)st.st_size, vec) == 0)
            for (uint64_t i = 0; i < pages; i++)
                resident += vec[i] & 1;
        free(vec);
        munmap(map, (size_t)st.st_size);
    }

    static char buffer[128 << 10];
    uint64_t bytes = 0, reads = 0;
    uint64_t start = now_ns();
    for (;;) {
        ssize_t got = read(fd, buffer, sizeof(buffer));
        if (got < 0)
            die("read");
        if (got == 0)
            break;
        bytes += (uint64_t)got;
        reads++;
    }
    uint64_t elapsed = now_ns() - start;
    close(fd);
    report_u64("elapsed_ns", elapsed);
    report_u64("ops", reads);
    report_u64("bytes", bytes);
    report_u64("pages", pages);
    report_u64("resident_before", resident);
}

/* --------------------------------------------------------------- threads */

enum kind { CONTENDED, ASYNC, ASYNC_MALLOC, ASYNC_SERIAL };

static pthread_mutex_t shared = PTHREAD_MUTEX_INITIALIZER;
static pthread_barrier_t barrier;
static uint64_t per_thread;
static enum kind kind;
static uint64_t counter;

static void *worker(void *arg) {
    (void)arg;
    pthread_barrier_wait(&barrier);
    uint64_t local = 0;
    for (uint64_t i = 0; i < per_thread; i++) {
        switch (kind) {
        case CONTENDED:
            pthread_mutex_lock(&shared);
            counter++;
            pthread_mutex_unlock(&shared);
            break;
        case ASYNC:
            local += (uint64_t)syscall(SYS_getppid);
            break;
        case ASYNC_MALLOC: {
            void *p = malloc(64);
            local += (uintptr_t)p;
            free(p);
            break;
        }
        case ASYNC_SERIAL:
            pthread_mutex_lock(&shared);
            local += (uint64_t)syscall(SYS_getppid);
            pthread_mutex_unlock(&shared);
            break;
        }
    }
    sink += local;
    return NULL;
}

static void threaded(enum kind which, uint64_t threads, uint64_t n) {
    kind = which;
    per_thread = n;
    pthread_t *ids = calloc(threads, sizeof(pthread_t));
    pthread_barrier_init(&barrier, NULL, (unsigned)threads + 1);
    for (uint64_t t = 0; t < threads; t++) {
        if (pthread_create(&ids[t], NULL, worker, NULL) != 0)
            die("pthread_create");
        pin(ids[t], (int)t);
    }
    uint64_t start = now_ns();
    pthread_barrier_wait(&barrier);
    for (uint64_t t = 0; t < threads; t++)
        pthread_join(ids[t], NULL);
    uint64_t elapsed = now_ns() - start;
    report_u64("elapsed_ns", elapsed);
    report_u64("ops", threads * n);
    report_u64("threads", threads);
    if (which == CONTENDED)
        report_u64("counter", counter);
    free(ids);
}

/* ----------------------------------------------------------------- pairs */

static void on_signal(int sig) { (void)sig; }

static void spin_until(uint64_t since, uint64_t gap) {
    while (now_ns() - since < gap)
        ;
}

/* Each pair is stamped immediately before each of its two events, on the
 * clock the tool stamps with. The tool's timestamps are then compared with
 * these pair by pair, so a pair the scheduler stretched is still compared with
 * the gap it actually had rather than with the one that was asked for. */
static void pairs(const char *what, uint64_t gap, uint64_t count) {
    uint64_t *stamps = calloc(count * 2, sizeof(uint64_t));
    long page = sysconf(_SC_PAGESIZE);
    char *region = NULL;
    pid_t tid = (pid_t)syscall(SYS_gettid);

    if (!strcmp(what, "pairs-signal")) {
        struct sigaction action = {0};
        action.sa_handler = on_signal;
        sigaction(SIGUSR1, &action, NULL);
    } else if (!strcmp(what, "pairs-fault")) {
        region = mmap(NULL, (size_t)(count * 2) * (size_t)page, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        if (region == MAP_FAILED)
            die("mmap");
        /* One fault per page, not one per folio: otherwise the second touch
         * of a pair can land in a folio the first one already mapped. */
        madvise(region, (size_t)(count * 2) * (size_t)page, MADV_NOHUGEPAGE);
    }

    for (uint64_t i = 0; i < count; i++) {
        for (int half = 0; half < 2; half++) {
            uint64_t t = now_ns();
            stamps[2 * i + (uint64_t)half] = t;
            if (!strcmp(what, "pairs-signal")) {
                syscall(SYS_tgkill, getpid(), tid, SIGUSR1);
            } else if (!strcmp(what, "pairs-malloc")) {
                void *p = malloc(half ? MARK_SECOND : MARK_FIRST);
                sink += (uintptr_t)p;
                free(p);
            } else {
                region[(2 * i + (uint64_t)half) * (uint64_t)page] = 1;
            }
            if (half == 0)
                spin_until(t, gap);
        }
        /* Pairs far enough apart that one can never be mistaken for the
         * neighbour of another, whatever the gap inside them. */
        usleep(2000);
    }

    report_u64("gap_ns", gap);
    report_u64("ops", count * 2);
    if (region)
        report_u64("region", (uint64_t)(uintptr_t)region);
    report_u64("page", (uint64_t)page);
    report_u64("mark_first", MARK_FIRST);
    report_u64("mark_second", MARK_SECOND);
    fputs(",\"stamps\":[", report);
    for (uint64_t i = 0; i < count * 2; i++)
        fprintf(report, "%s%llu", i ? "," : "", (unsigned long long)stamps[i]);
    fputs("]", report);
    free(stamps);
}

/* ------------------------------------------------------------------ main */

static uint64_t arg(int argc, char **argv, int index) {
    if (index >= argc) {
        fprintf(stderr, "workload %s: missing argument %d\n", argv[1], index - 1);
        exit(2);
    }
    return strtoull(argv[index], NULL, 10);
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: workload MODE ARGS...\n");
        return 2;
    }
    const char *mode = argv[1];
    parse_cpus();
    pin(pthread_self(), 0);
    report_open(mode);

    if (!strcmp(mode, "launch")) {
        /* nothing: the cost is in getting here */
    } else if (!strcmp(mode, "spawn")) {
        spawn(arg(argc, argv, 2));
    } else if (!strcmp(mode, "compute")) {
        compute(arg(argc, argv, 2));
    } else if (!strcmp(mode, "memory")) {
        memory(arg(argc, argv, 2), arg(argc, argv, 3));
    } else if (!strcmp(mode, "syscall")) {
        syscalls(arg(argc, argv, 2));
    } else if (!strcmp(mode, "io-prepare")) {
        if (argc < 4)
            return 2;
        io_prepare(argv[2], arg(argc, argv, 3));
    } else if (!strcmp(mode, "io")) {
        if (argc < 3)
            return 2;
        io(argv[2]);
    } else if (!strcmp(mode, "contended")) {
        threaded(CONTENDED, arg(argc, argv, 2), arg(argc, argv, 3));
    } else if (!strcmp(mode, "async")) {
        threaded(ASYNC, arg(argc, argv, 2), arg(argc, argv, 3));
    } else if (!strcmp(mode, "async-malloc")) {
        threaded(ASYNC_MALLOC, arg(argc, argv, 2), arg(argc, argv, 3));
    } else if (!strcmp(mode, "async-serial")) {
        threaded(ASYNC_SERIAL, arg(argc, argv, 2), arg(argc, argv, 3));
    } else if (!strncmp(mode, "pairs-", 6)) {
        pairs(mode, arg(argc, argv, 2), arg(argc, argv, 3));
    } else {
        fprintf(stderr, "workload: unknown mode `%s`\n", mode);
        return 2;
    }
    report_close();
    return 0;
}
