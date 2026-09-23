/* BEGIN SERVER -- omn's vertical-case server.
 *
 *   server <file> <lock_ms>:<reads> ...
 *
 * A real TCP server on loopback, a client thread that measures each request's
 * latency on its own clock, and a holder thread that takes the server's lock
 * and sleeps under it. Each plan item is one request: the worker waits for the
 * lock for `lock_ms` (a `nanosleep` under the mutex, by the holder), then reads
 * `reads` pages of `file` with O_DIRECT after dropping it from the page cache,
 * so each read is one block request. A `lock_ms` of -1 abandons the request:
 * it is declared at its start and never ended, which is the loss the tool
 * must count rather than hide. Every request is declared with USDT
 * probes, `omn:request__start(id)` and `omn:request__done(id)`, and the server
 * and the client print what their own clocks saw on stderr, prefixed `vertical:`. */
#define _GNU_SOURCE
#include <sys/sdt.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define PAGE 4096
#define STRIDE (64 * 1024)
#define MAX_PLAN 64
/* The largest value an eventfd holds, which is how the holder is told to stop. */
#define QUIT 0xfffffffffffffffeULL

static long plan_lock[MAX_PLAN], plan_reads[MAX_PLAN];
static int plan_n, port, hold_fd, held_fd;
static const char *path;
static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t out = PTHREAD_MUTEX_INITIALIZER;

static long now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000000000L + ts.tv_nsec;
}
static int tid(void) { return (int)syscall(SYS_gettid); }
#define SAY(...) do { pthread_mutex_lock(&out); fprintf(stderr, "vertical: " __VA_ARGS__); \
                      fflush(stderr); pthread_mutex_unlock(&out); } while (0)

/* Takes the lock when asked, says it has it, and sleeps under it. */
static void *holder(void *arg) {
    (void)arg;
    SAY("role=holder tid=%d\n", tid());
    for (;;) {
        uint64_t ms;
        if (read(hold_fd, &ms, sizeof ms) != sizeof ms || ms == QUIT) return NULL;
        pthread_mutex_lock(&lock);
        uint64_t one = 1;
        (void)!write(held_fd, &one, sizeof one);
        long t0 = now_ns();
        struct timespec d = { (time_t)(ms / 1000), (long)(ms % 1000) * 1000000L };
        nanosleep(&d, NULL);
        long held = now_ns() - t0;
        pthread_mutex_unlock(&lock);
        SAY("role=holder held_ns=%ld\n", held);
    }
}

/* One request at a time over a fresh connection, timed from before the send to
 * the last byte of the response. */
static void *client(void *arg) {
    (void)arg;
    SAY("role=client tid=%d\n", tid());
    for (int i = 0; i < plan_n; i++) {
        struct timespec gap = { 0, 50 * 1000000L };
        nanosleep(&gap, NULL);
        int s = socket(AF_INET, SOCK_STREAM, 0);
        struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
        a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        if (connect(s, (struct sockaddr *)&a, sizeof a)) { perror("connect"); exit(1); }
        char request[128];
        int n = snprintf(request, sizeof request,
                         "GET /?lock=%ld&reads=%ld HTTP/1.1\r\nHost: omn\r\n\r\n",
                         plan_lock[i], plan_reads[i]);
        long t0 = now_ns();
        (void)!send(s, request, n, 0);
        char buf[512];
        long got = 0, r;
        while ((r = recv(s, buf, sizeof buf, 0)) > 0) got += r;
        long t1 = now_ns();
        close(s);
        SAY("role=client req=%x client_ns=%ld sent_ns=%ld bytes=%ld\n", 0xa001 + i, t1 - t0, t0, got);
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 3) { fprintf(stderr, "usage: server <file> <lock_ms>:<reads> ...\n"); return 2; }
    path = argv[1];
    for (int i = 2; i < argc && plan_n < MAX_PLAN; i++, plan_n++)
        if (sscanf(argv[i], "%ld:%ld", &plan_lock[plan_n], &plan_reads[plan_n]) != 2) return 2;

    /* The file every read comes from: incompressible, on the device, and out of
     * the page cache -- which is what makes one O_DIRECT page one request. */
    long max_reads = 0;
    for (int i = 0; i < plan_n; i++) if (plan_reads[i] > max_reads) max_reads = plan_reads[i];
    int f = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    int random = open("/dev/urandom", O_RDONLY);
    char *chunk = malloc(STRIDE);
    for (long i = 0; i < (max_reads > 0 ? max_reads : 1); i++) {
        (void)!read(random, chunk, STRIDE);
        (void)!write(f, chunk, STRIDE);
    }
    fsync(f);
    close(random);
    close(f);

    hold_fd = eventfd(0, 0);
    held_fd = eventfd(0, 0);
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    int yes = 1;
    setsockopt(listener, SOL_SOCKET, SO_REUSEADDR, &yes, sizeof yes);
    struct sockaddr_in a = { .sin_family = AF_INET };
    a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    bind(listener, (struct sockaddr *)&a, sizeof a);
    socklen_t len = sizeof a;
    getsockname(listener, (struct sockaddr *)&a, &len);
    port = ntohs(a.sin_port);
    listen(listener, 16);
    SAY("role=worker tid=%d pid=%d port=%d\n", tid(), getpid(), port);

    pthread_t h, c;
    pthread_create(&h, NULL, holder, NULL);
    pthread_create(&c, NULL, client, NULL);

    char *page = aligned_alloc(PAGE, PAGE);
    for (int i = 0; i < plan_n; i++) {
        int conn = accept(listener, NULL, NULL);
        char buf[512];
        long got = 0, r;
        while (got < (long)sizeof buf - 1 && (r = recv(conn, buf + got, sizeof buf - 1 - got, 0)) > 0) {
            got += r;
            buf[got] = 0;
            if (strstr(buf, "\r\n\r\n")) break;
        }
        long lock_ms = 0, reads = 0;
        sscanf(buf, "GET /?lock=%ld&reads=%ld", &lock_ms, &reads);
        unsigned long id = 0xa001 + i;

        long born = now_ns();
        DTRACE_PROBE1(omn, request__start, id);

        int abandoned = lock_ms < 0;
        long lock_ns = 0;
        if (lock_ms > 0) {
            long t0 = now_ns();
            uint64_t ms = (uint64_t)lock_ms, one;
            (void)!write(hold_fd, &ms, sizeof ms);
            (void)!read(held_fd, &one, sizeof one);
            pthread_mutex_lock(&lock);
            lock_ns = now_ns() - t0;
            pthread_mutex_unlock(&lock);
        }

        long read_ns = 0, done_reads = 0;
        if (reads > 0) {
            int fd = open(path, O_RDONLY);
            posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
            close(fd);
            fd = open(path, O_RDONLY | O_DIRECT);
            long t0 = now_ns();
            for (long k = 0; k < reads; k++)
                if (pread(fd, page, PAGE, k * STRIDE) == PAGE) done_reads++;
            read_ns = now_ns() - t0;
            close(fd);
        }

        const char response[] = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";
        (void)!send(conn, response, sizeof response - 1, 0);
        if (!abandoned) DTRACE_PROBE1(omn, request__done, id);
        long ended = now_ns();
        close(conn);
        SAY("role=worker req=%lx tid=%d lock_ms=%ld reads=%ld done_reads=%ld lock_ns=%ld "
            "read_ns=%ld span_ns=%ld born_ns=%ld abandoned=%d\n",
            id, tid(), lock_ms, reads, done_reads, lock_ns, read_ns, ended - born, born, abandoned);
    }
    pthread_join(c, NULL);
    uint64_t quit = QUIT;
    (void)!write(hold_fd, &quit, sizeof quit);
    pthread_join(h, NULL);
    unlink(path);
    return 0;
}
/* END SERVER */
