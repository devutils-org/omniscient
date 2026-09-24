/*
 * The triage layer's ground truth: one program whose limit the caller chose.
 *
 * Built by `tests/triage.rs` into `target/omn-triage-fixture/workload`, which
 * is the path `examples/triage-*.yml` launch. Every mode spends its time in a
 * way the caller picked -- on a cpu, blocked on a disk, waiting for a mutex,
 * queued behind its own threads -- and prints the kernel's accounting of its
 * main thread on stderr, prefixed `omn-truth`, before it exits. The tool reads
 * none of that line.
 *
 *   workload cpu MS                    spin MS of the thread's own cpu time
 *   workload io N DIR                  N x (write 4 KiB, fdatasync) in DIR
 *   workload lock N HOLD_US            take a mutex a helper holds HOLD_US at a time
 *   workload tail COUNTER EVERY MS BLOCK_MS
 *                                      spin MS; every EVERY-th run also sleep BLOCK_MS
 *   workload queue THREADS MS          THREADS threads spin MS each, all on one cpu
 *   workload diff FACTOR               four functions of equal cost, `beta` FACTOR times
 *
 * The spin is a chain of dependent multiplies: its speed is the multiplier's
 * latency, which an SMT sibling does not share, so a unit of work costs about
 * the same number of cycles on a loaded machine as on an idle one.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

static volatile uint64_t sink;

static uint64_t now_ns(clockid_t clock)
{
	struct timespec ts;
	clock_gettime(clock, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

/* A fixed amount of work: `units` rounds of a dependent multiply chain.
 * Always inlined, so the cost is the named function's own and a profile's
 * leaf frame is the function that asked for the work. */
__attribute__((always_inline)) static inline uint64_t work(uint64_t units)
{
	uint64_t x = sink | 1;
	for (uint64_t i = 0; i < units * 1000; i++)
		x = x * 6364136223846793005ull + 1442695040888963407ull;
	return x;
}

/* Spins until this thread has spent `ms` of its own cpu time. */
static void spin_cpu_ms(uint64_t ms)
{
	uint64_t until = now_ns(CLOCK_THREAD_CPUTIME_ID) + ms * 1000000ull;
	while (now_ns(CLOCK_THREAD_CPUTIME_ID) < until)
		sink ^= work(10);
}

/* The kernel's accounting of this thread, and the wall clock since `start`. */
static void truth(const char *mode, uint64_t start, const char *extra)
{
	uint64_t wall = now_ns(CLOCK_MONOTONIC) - start;
	unsigned long long on = 0, rq = 0, slices = 0;
	FILE *f = fopen("/proc/thread-self/schedstat", "r");
	if (f) {
		if (fscanf(f, "%llu %llu %llu", &on, &rq, &slices) != 3)
			on = rq = slices = 0;
		fclose(f);
	}
	fprintf(stderr, "omn-truth mode=%s tid=%d wall_ns=%llu oncpu_ns=%llu runq_ns=%llu%s%s\n",
		mode, getpid(), (unsigned long long)wall, on, rq, extra ? " " : "",
		extra ? extra : "");
}

/* ---------------------------------------------------------------- io */

static int mode_io(int n, const char *dir)
{
	char path[4096];
	snprintf(path, sizeof path, "%s/omn-triage-io.%d", dir, getpid());
	int fd = open(path, O_CREAT | O_TRUNC | O_WRONLY, 0600);
	if (fd < 0) {
		fprintf(stderr, "cannot create %s: %s\n", path, strerror(errno));
		return 1;
	}
	unlink(path);
	char block[4096];
	memset(block, 'x', sizeof block);
	uint64_t start = now_ns(CLOCK_MONOTONIC);
	uint64_t synced = 0;
	for (int i = 0; i < n; i++) {
		if (pwrite(fd, block, sizeof block, (off_t)i * sizeof block) != sizeof block)
			return 1;
		uint64_t before = now_ns(CLOCK_MONOTONIC);
		if (fdatasync(fd) != 0)
			return 1;
		synced += now_ns(CLOCK_MONOTONIC) - before;
	}
	close(fd);
	char extra[64];
	snprintf(extra, sizeof extra, "sync_ns=%llu syncs=%d", (unsigned long long)synced, n);
	truth("io", start, extra);
	return 0;
}

/* ---------------------------------------------------------------- lock */

static pthread_mutex_t mutex = PTHREAD_MUTEX_INITIALIZER;
static volatile int stop;
static uint64_t hold_us;

static void *holder(void *arg)
{
	(void)arg;
	while (!stop) {
		pthread_mutex_lock(&mutex);
		uint64_t until = now_ns(CLOCK_MONOTONIC) + hold_us * 1000;
		while (now_ns(CLOCK_MONOTONIC) < until)
			sink ^= work(1);
		pthread_mutex_unlock(&mutex);
		/* Long enough for the waiter to be woken and take the mutex:
		 * without it the holder takes it straight back and the waiter
		 * starves rather than contends. */
		usleep(50);
	}
	return NULL;
}

static int mode_lock(int n, uint64_t hold)
{
	hold_us = hold;
	pthread_t thread;
	/* Taken before the holder exists and released once it is running,
	 * so the first acquisition is contended like every other. */
	pthread_mutex_lock(&mutex);
	pthread_create(&thread, NULL, holder, NULL);
	usleep(1000);
	pthread_mutex_unlock(&mutex);
	usleep(100);
	uint64_t start = now_ns(CLOCK_MONOTONIC);
	uint64_t waited = 0;
	for (int i = 0; i < n; i++) {
		uint64_t before = now_ns(CLOCK_MONOTONIC);
		pthread_mutex_lock(&mutex);
		waited += now_ns(CLOCK_MONOTONIC) - before;
		sink ^= work(1);
		pthread_mutex_unlock(&mutex);
		/* Gives the holder its turn: a mutex released and retaken at
		 * once is retaken by the same thread. Short beside HOLD_US, and
		 * a wait of its own that is not the mutex's. */
		usleep(20);
	}
	stop = 1;
	char extra[64];
	snprintf(extra, sizeof extra, "lock_wait_ns=%llu locks=%d", (unsigned long long)waited, n);
	truth("lock", start, extra);
	pthread_join(thread, NULL);
	return 0;
}

/* ---------------------------------------------------------------- tail */

static int mode_tail(const char *counter, int every, uint64_t ms, uint64_t block_ms)
{
	uint64_t start = now_ns(CLOCK_MONOTONIC);
	int run = 0;
	FILE *f = fopen(counter, "r");
	if (f) {
		if (fscanf(f, "%d", &run) != 1)
			run = 0;
		fclose(f);
	}
	run += 1;
	f = fopen(counter, "w");
	if (!f)
		return 1;
	fprintf(f, "%d\n", run);
	fclose(f);
	spin_cpu_ms(ms);
	int slow = every > 0 && run % every == 0;
	if (slow)
		usleep(block_ms * 1000);
	char extra[64];
	snprintf(extra, sizeof extra, "run=%d slow=%d", run, slow);
	truth("tail", start, extra);
	return 0;
}

/* ---------------------------------------------------------------- queue */

static uint64_t queue_ms;

static void *spinner(void *arg)
{
	(void)arg;
	spin_cpu_ms(queue_ms);
	return NULL;
}

static int mode_queue(int threads, uint64_t ms)
{
	cpu_set_t allowed, one;
	CPU_ZERO(&one);
	if (sched_getaffinity(0, sizeof allowed, &allowed) == 0) {
		for (int cpu = 0; cpu < CPU_SETSIZE; cpu++)
			if (CPU_ISSET(cpu, &allowed)) {
				CPU_SET(cpu, &one);
				break;
			}
		/* Inherited by every thread created after it. */
		sched_setaffinity(0, sizeof one, &one);
	}
	queue_ms = ms;
	uint64_t start = now_ns(CLOCK_MONOTONIC);
	pthread_t pool[64];
	if (threads > 64)
		threads = 64;
	for (int i = 1; i < threads; i++)
		pthread_create(&pool[i], NULL, spinner, NULL);
	spin_cpu_ms(ms);
	char extra[64];
	snprintf(extra, sizeof extra, "threads=%d", threads);
	truth("queue", start, extra);
	for (int i = 1; i < threads; i++)
		pthread_join(pool[i], NULL);
	return 0;
}

/* ---------------------------------------------------------------- diff */

/* Four functions of equal cost, told apart only by their names. */
#define UNITS 110000
__attribute__((noinline)) static void alpha(void) { sink ^= work(UNITS); }
__attribute__((noinline)) static void beta(void) { sink ^= work(UNITS); }
__attribute__((noinline)) static void gamma_(void) { sink ^= work(UNITS); }
__attribute__((noinline)) static void delta(void) { sink ^= work(UNITS); }

static int mode_diff(int factor)
{
	uint64_t start = now_ns(CLOCK_MONOTONIC);
	alpha();
	for (int i = 0; i < factor; i++)
		beta();
	gamma_();
	delta();
	char extra[32];
	snprintf(extra, sizeof extra, "beta_factor=%d", factor);
	truth("diff", start, extra);
	return 0;
}

int main(int argc, char **argv)
{
	if (argc >= 3 && strcmp(argv[1], "cpu") == 0) {
		uint64_t start = now_ns(CLOCK_MONOTONIC);
		spin_cpu_ms(strtoull(argv[2], NULL, 10));
		truth("cpu", start, NULL);
		return 0;
	}
	if (argc >= 4 && strcmp(argv[1], "io") == 0)
		return mode_io(atoi(argv[2]), argv[3]);
	if (argc >= 4 && strcmp(argv[1], "lock") == 0)
		return mode_lock(atoi(argv[2]), strtoull(argv[3], NULL, 10));
	if (argc >= 6 && strcmp(argv[1], "tail") == 0)
		return mode_tail(argv[2], atoi(argv[3]), strtoull(argv[4], NULL, 10),
				 strtoull(argv[5], NULL, 10));
	if (argc >= 4 && strcmp(argv[1], "queue") == 0)
		return mode_queue(atoi(argv[2]), strtoull(argv[3], NULL, 10));
	if (argc >= 3 && strcmp(argv[1], "diff") == 0)
		return mode_diff(atoi(argv[2]));
	fprintf(stderr, "usage: workload cpu MS | io N DIR | lock N HOLD_US | "
			"tail COUNTER EVERY MS BLOCK_MS | queue THREADS MS | diff FACTOR\n");
	return 2;
}
