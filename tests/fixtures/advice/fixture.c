/*
 * The advice rules' ground truth (tests/advice_truth.rs). Each mode is a
 * workload one rule fires on; the test changes the setting the rule names --
 * in the fixture or in the scope it runs in, never on the machine -- and runs
 * it again.
 *
 *   grow MB              allocate and touch MB of incompressible memory
 *   hold MB SECONDS      allocate MB, then sleep
 *   oomtwo DIR ADJ       run DIR/keeper (hold 120 MB) and DIR/grower (grow
 *                        100 MB, with oom_score_adj ADJ), wait for both
 *   spin THREADS ITERS   THREADS threads, each doing ITERS units of work
 *   fragment MB SECONDS  touch MB in small pages, then give back every other
 *                        page: memory full of half-used 2 MiB blocks, held
 *                        SECONDS (a guest's; never run on the development
 *                        machine, which has too much memory to fragment)
 *   huge MB              fault MB of anonymous memory 2 MiB at a time, which
 *                        transparent huge pages back where the policy lets
 *   readseq PATH MB HINT write PATH (MB of incompressible bytes), evict it
 *                        from the cache, and read its first half 4 KiB at
 *                        a time; HINT random or normal. Half, because over
 *                        a whole file pages filled and pages looked up
 *                        agree whatever readahead did (io.readahead).
 *   swapcold MB FILE N   fill MB of anonymous memory, each page its own
 *                        bytes, read FILE whole N times, then touch the
 *                        memory again and print how long that took: a
 *                        program's cold memory against a stream of page
 *                        cache, which reclaim chooses between by
 *                        vm.swappiness (a guest's; never the development
 *                        machine's)
 *   backlog MB           write MB over a loopback TCP connection to a reader
 *                        that takes 64 KiB every 5 ms; every chunk carries
 *                        the time it was handed to write(), and the reader
 *                        prints the longest it waited to arrive. Run in a
 *                        network namespace of its own: tcp_notsent_lowat is
 *                        the namespace's.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <netinet/in.h>
#include <net/if.h>
#include <netinet/tcp.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <time.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static volatile uint64_t *volatile keep;
static volatile uint64_t sink;

static void grow(long mb)
{
	uint64_t x = 88172645463325252ull;
	for (long i = 0; i < mb; i++) {
		uint64_t *page = malloc(1 << 20);
		if (!page)
			exit(3);
		for (int j = 0; j < (1 << 17); j++) {
			x ^= x << 13;
			x ^= x >> 7;
			x ^= x << 17;
			page[j] = x;
		}
		keep = page;
	}
}

static long iterations;

static void *work(void *arg)
{
	(void)arg;
	uint64_t x = 1;
	for (long i = 0; i < iterations; i++)
		x = x * 6364136223846793005ull + 1442695040888963407ull;
	sink = x;
	return NULL;
}

static pid_t run(const char *path, char *const argv[], int adj)
{
	pid_t pid = fork();
	if (pid == 0) {
		if (adj) {
			FILE *file = fopen("/proc/self/oom_score_adj", "w");
			if (!file || fprintf(file, "%d\n", adj) < 0 || fclose(file) != 0)
				_exit(4);
		}
		execv(path, argv);
		_exit(5);
	}
	return pid;
}

static uint64_t now_ns(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (uint64_t)ts.tv_sec * 1000000000ull + (uint64_t)ts.tv_nsec;
}

#define CHUNK (64 << 10)

static long backlog_chunks;
static int backlog_fd;

/* The slow reader: 64 KiB every 5 ms, each chunk's stamp compared with now. */
static void *backlog_read(void *arg)
{
	(void)arg;
	static char chunk[CHUNK];
	uint64_t worst = 0;
	for (long i = 0; i < backlog_chunks; i++) {
		size_t got = 0;
		while (got < CHUNK) {
			ssize_t n = read(backlog_fd, chunk + got, CHUNK - got);
			if (n <= 0)
				return NULL;
			got += (size_t)n;
		}
		uint64_t sent;
		memcpy(&sent, chunk, sizeof sent);
		uint64_t waited = now_ns() - sent;
		if (waited > worst)
			worst = waited;
		usleep(5000);
	}
	printf("fixture: %ld chunks, the longest from write() to read() %.1f ms\n", backlog_chunks,
	       worst / 1e6);
	return NULL;
}

static int backlog(long mb)
{
	/* A new network namespace's loopback is down; its root may bring it up. */
	int control = socket(AF_INET, SOCK_DGRAM, 0);
	struct ifreq lo = {.ifr_name = "lo"};
	if (control >= 0 && !ioctl(control, SIOCGIFFLAGS, &lo) && !(lo.ifr_flags & IFF_UP)) {
		lo.ifr_flags |= IFF_UP;
		ioctl(control, SIOCSIFFLAGS, &lo);
	}
	if (control >= 0)
		close(control);
	int listener = socket(AF_INET, SOCK_STREAM, 0);
	struct sockaddr_in addr = {.sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK)};
	socklen_t len = sizeof addr;
	if (listener < 0 || bind(listener, (struct sockaddr *)&addr, len) || listen(listener, 1) ||
	    getsockname(listener, (struct sockaddr *)&addr, &len))
		return 8;
	int writer = socket(AF_INET, SOCK_STREAM, 0);
	if (writer < 0 || connect(writer, (struct sockaddr *)&addr, len))
		return 9;
	backlog_fd = accept(listener, NULL, NULL);
	if (backlog_fd < 0)
		return 10;
	backlog_chunks = (mb << 20) / CHUNK;
	pthread_t reader;
	pthread_create(&reader, NULL, backlog_read, NULL);
	static char chunk[CHUNK];
	for (long i = 0; i < backlog_chunks; i++) {
		uint64_t stamp = now_ns();
		memcpy(chunk, &stamp, sizeof stamp);
		size_t put = 0;
		while (put < CHUNK) {
			ssize_t n = write(writer, chunk + put, CHUNK - put);
			if (n <= 0)
				return 11;
			put += (size_t)n;
		}
	}
	pthread_join(reader, NULL);
	close(writer);
	return 0;
}

static int swapcold(long mb, const char *path, int passes)
{
	size_t size = (size_t)mb << 20, page = 4096;
	uint64_t *memory = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
	if (memory == MAP_FAILED)
		return 12;
	uint64_t x = 0x9e3779b97f4a7c15ull;
	for (size_t i = 0; i < size / 8; i++) {
		x ^= x << 13;
		x ^= x >> 7;
		x ^= x << 17;
		memory[i] = x;
	}
	static char block[1 << 20];
	uint64_t started = now_ns();
	for (int pass = 0; pass < passes; pass++) {
		int fd = open(path, O_RDONLY);
		if (fd < 0)
			return 13;
		while (read(fd, block, sizeof block) > 0)
			;
		close(fd);
	}
	uint64_t streamed = now_ns();
	uint64_t sum = 0;
	for (size_t i = 0; i < size / 8; i += page / 8)
		sum += memory[i];
	sink = sum;
	uint64_t touched = now_ns();
	printf("fixture: %d passes over the file in %.0f ms, %ld MiB touched again in %.1f ms\n", passes,
	       (streamed - started) / 1e6, mb, (touched - streamed) / 1e6);
	return 0;
}

int main(int argc, char **argv)
{
	if (argc >= 4 && !strcmp(argv[1], "fragment")) {
		size_t bytes = (size_t)atol(argv[2]) << 20, page = 4096;
		char *area = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		if (area == MAP_FAILED)
			return 8;
		madvise(area, bytes, MADV_NOHUGEPAGE);
		for (size_t at = 0; at < bytes; at += page)
			area[at] = 1;
		for (size_t at = 0; at < bytes; at += 2 * page)
			madvise(area + at, page, MADV_DONTNEED);
		printf("fixture: fragmented %s MB\n", argv[2]);
		fflush(stdout);
		sleep(atoi(argv[3]));
		return 0;
	}
	if (argc >= 3 && !strcmp(argv[1], "huge")) {
		size_t bytes = (size_t)atol(argv[2]) << 20, step = 2 << 20;
		char *area = mmap(NULL, bytes + step, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
		if (area == MAP_FAILED)
			return 9;
		char *aligned = (char *)(((uintptr_t)area + step - 1) & ~(uintptr_t)(step - 1));
		for (size_t at = 0; at < bytes; at += step)
			aligned[at] = 1;
		printf("fixture: faulted %s MB\n", argv[2]);
		return 0;
	}
	if (argc >= 3 && !strcmp(argv[1], "grow")) {
		grow(atol(argv[2]));
		printf("fixture: grew %s MB\n", argv[2]);
		return 0;
	}
	if (argc >= 4 && !strcmp(argv[1], "hold")) {
		grow(atol(argv[2]));
		sleep(atoi(argv[3]));
		return 0;
	}
	if (argc >= 4 && !strcmp(argv[1], "oomtwo")) {
		char keeper[4096], grower[4096];
		snprintf(keeper, sizeof keeper, "%s/keeper", argv[2]);
		snprintf(grower, sizeof grower, "%s/grower", argv[2]);
		char *hold_argv[] = { keeper, "hold", "120", "30", NULL };
		char *grow_argv[] = { grower, "grow", "100", NULL };
		pid_t held = run(keeper, hold_argv, 0);
		usleep(500 * 1000);
		pid_t grown = run(grower, grow_argv, atoi(argv[3]));
		int status;
		/* Whichever ends first, the other is not needed. */
		pid_t first = wait(&status);
		if (WIFSIGNALED(status))
			printf("fixture: %s was killed\n", first == held ? "keeper" : "grower");
		kill(first == held ? grown : held, SIGTERM);
		wait(&status);
		return 0;
	}
	if (argc >= 4 && !strcmp(argv[1], "spin")) {
		int threads = atoi(argv[2]);
		iterations = atol(argv[3]);
		pthread_t thread[64];
		for (int i = 0; i < threads && i < 64; i++)
			pthread_create(&thread[i], NULL, work, NULL);
		for (int i = 0; i < threads && i < 64; i++)
			pthread_join(thread[i], NULL);
		return 0;
	}
	if (argc >= 5 && !strcmp(argv[1], "readseq")) {
		long mb = atol(argv[3]);
		char block[4096];
		uint64_t x = 0x9e3779b97f4a7c15ull;
		int fd = open(argv[2], O_RDWR | O_CREAT, 0600);
		if (fd < 0)
			return 6;
		off_t size = lseek(fd, 0, SEEK_END);
		if (size < mb << 20) {
			for (long i = 0; i < (mb << 20) / (long)sizeof block; i++) {
				/* A filesystem that compresses would read a run of one
				 * byte from a tenth of the blocks. */
				for (size_t j = 0; j < sizeof block; j += 8) {
					x ^= x << 13;
					x ^= x >> 7;
					x ^= x << 17;
					memcpy(block + j, &x, 8);
				}
				if (write(fd, block, sizeof block) != sizeof block)
					return 7;
			}
		}
		fsync(fd);
		/* The cache this file is in is this program's to drop. */
		posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
		close(fd);
		fd = open(argv[2], O_RDONLY);
		if (!strcmp(argv[4], "random"))
			posix_fadvise(fd, 0, 0, POSIX_FADV_RANDOM);
		long reads = 0, half = (mb << 20) / 2 / (long)sizeof block;
		while (reads < half && read(fd, block, sizeof block) == sizeof block)
			reads++;
		printf("fixture: %ld reads\n", reads);
		close(fd);
		return 0;
	}
	if (argc >= 5 && !strcmp(argv[1], "swapcold"))
		return swapcold(atol(argv[2]), argv[3], atoi(argv[4]));
	if (argc >= 3 && !strcmp(argv[1], "backlog"))
		return backlog(atol(argv[2]));
	fprintf(stderr, "usage: see the comment at the top\n");
	return 2;
}
