/* BEGIN SERVER -- omn's vertical-case server.
 *
 *   server <file> <lock_ms>:<reads>[:<gap_ms>[:<body>]] ...
 *
 * A real TCP server on loopback, a client thread that measures each request's
 * latency on its own clock, and a holder thread that takes the server's lock
 * and sleeps under it. Each plan item is one request: the worker waits for the
 * lock for `lock_ms` (a `nanosleep` under the mutex, by the holder), then reads
 * `reads` pages of `file` with O_DIRECT after dropping it from the page cache,
 * so each read is one block request. A `lock_ms` of -1 abandons the request:
 * it is declared at its start and never ended, which is the loss the tool
 * must count rather than hide.
 *
 * The two network injections are the client's, and need no root. A `gap_ms`
 * makes the client send the first half of its request, sleep, and send the
 * rest: the server cannot declare a request it has not read, so the pause is
 * between the request's first byte and its birth. A `body` makes the server
 * answer with that many bytes through a small send buffer, to a client with a
 * small receive buffer that sleeps a millisecond after every read: the send
 * blocks on the closed window, and the last bytes leave the stack well after
 * the server has declared the request done. A negative `body` is a client that
 * never reads the answer and closes: the response never leaves the stack,
 * which is the loss the tool must say rather than hide.
 *
 * Every request is declared with USDT probes, `omn:request__start(id, fd)` and
 * `omn:request__done(id, fd)`, the descriptor being the connection's, and the
 * server and the client print what their own clocks saw on stderr, prefixed
 * `vertical:`. */
#define _GNU_SOURCE
/* Built with -DGUARDED, both probes are behind a semaphore, as a program that
 * wants its probes to cost nothing when nobody listens declares them: the site
 * is skipped unless the semaphore is raised, which the tool asks the kernel to
 * do. */
#ifdef GUARDED
#define _SDT_HAS_SEMAPHORES 1
unsigned short omn_request__start_semaphore __attribute__((section(".probes"))) = 0;
unsigned short omn_request__done_semaphore __attribute__((section(".probes"))) = 0;
#define START_ENABLED() (omn_request__start_semaphore != 0)
#define DONE_ENABLED() (omn_request__done_semaphore != 0)
#else
#define START_ENABLED() 1
#define DONE_ENABLED() 1
#endif
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
/* Requests a plan may hold: a few hundred is what a recording asks for. */
#define MAX_PLAN 1024
#define MAX_READS 4096
/* The slow reader's buffers: the client's receive buffer as it asks for it, the
 * server's send buffer, and the pause after each read. */
#define SLOW_RCVBUF 4096
#define SLOW_SNDBUF 16384
#define SLOW_READ_PAUSE_NS 1000000L
/* The largest value an eventfd holds, which is how the holder is told to stop. */
#define QUIT 0xfffffffffffffffeULL

static long plan_lock[MAX_PLAN], plan_reads[MAX_PLAN], plan_gap[MAX_PLAN], plan_body[MAX_PLAN];
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

static void pause_ns(long ns) {
    struct timespec d = { (time_t)(ns / 1000000000L), ns % 1000000000L };
    nanosleep(&d, NULL);
}

/* One request at a time over a fresh connection, timed from before the first
 * send to the last byte of the response. */
static void *client(void *arg) {
    (void)arg;
    SAY("role=client tid=%d\n", tid());
    static long read_start[MAX_READS], read_end[MAX_READS], read_total[MAX_READS];
    static char buf[64 * 1024];
    for (int i = 0; i < plan_n; i++) {
        pause_ns(50 * 1000000L);
        int s = socket(AF_INET, SOCK_STREAM, 0);
        int slow = plan_body[i] != 0, unread = plan_body[i] < 0, rcvbuf = 0;
        if (slow) {
            /* Before the connect, so the window the handshake offers is this
             * one, and locked: the kernel no longer grows a buffer set by hand. */
            int want = SLOW_RCVBUF;
            setsockopt(s, SOL_SOCKET, SO_RCVBUF, &want, sizeof want);
        }
        socklen_t len = sizeof rcvbuf;
        getsockopt(s, SOL_SOCKET, SO_RCVBUF, &rcvbuf, &len);
        struct sockaddr_in a = { .sin_family = AF_INET, .sin_port = htons(port) };
        a.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        if (connect(s, (struct sockaddr *)&a, sizeof a)) { perror("connect"); exit(1); }
        char request[160];
        int n = snprintf(request, sizeof request,
                         "GET /?lock=%ld&reads=%ld&body=%ld HTTP/1.1\r\nHost: omn\r\n\r\n",
                         plan_lock[i], plan_reads[i], plan_body[i]);
        long t0 = now_ns(), first_end = 0, second = 0;
        if (plan_gap[i] > 0) {
            int half = n / 2;
            (void)!send(s, request, half, 0);
            first_end = now_ns();
            pause_ns(plan_gap[i] * 1000000L);
            second = now_ns();
            (void)!send(s, request + half, n - half, 0);
        } else {
            (void)!send(s, request, n, 0);
            first_end = now_ns();
        }
        long got = 0, r, slept = 0, expect = -1;
        int reads = 0;
        if (unread) {
            /* Long enough for the server to have answered, then gone without
             * reading a byte: the close with unread data resets the connection. */
            pause_ns(20 * 1000000L);
            close(s);
            SAY("role=client req=%x unread=1\n", 0xa001 + i);
            continue;
        }
        for (;;) {
            long rs = now_ns();
            r = recv(s, buf, slow ? sizeof buf : 512, 0);
            long re = now_ns();
            if (r <= 0) break;
            got += r;
            if (reads < MAX_READS) {
                read_start[reads] = rs;
                read_end[reads] = re;
                read_total[reads] = got;
            }
            reads++;
            if (expect < 0) {
                char *end = memmem(buf, r, "\r\n\r\n", 4);
                long length = 0;
                if (end && sscanf(buf, "HTTP/1.1 200 OK\r\nContent-Length: %ld", &length) == 1)
                    expect = (end + 4 - buf) + length;
            }
            /* No pause once the whole answer is in: what is left is the close. */
            if (slow && got < expect) {
                long p0 = now_ns();
                pause_ns(SLOW_READ_PAUSE_NS);
                slept += now_ns() - p0;
            }
        }
        long t1 = now_ns();
        close(s);
        /* The earliest the response's last byte can have left the server: the
         * start of the first read after which fewer than a receive buffer of
         * bytes were left. Before it, more than a buffer was unread, and the
         * server cannot have sent past the window a buffer allows. */
        long room = 0, last = 0;
        int kept = reads < MAX_READS ? reads : MAX_READS;
        for (int k = 0; k < kept; k++)
            if (read_total[k] >= got - rcvbuf) { room = read_start[k]; break; }
        if (kept > 0) last = read_end[kept - 1];
        SAY("role=client req=%x client_ns=%ld sent_ns=%ld first_end_ns=%ld second_ns=%ld "
            "bytes=%ld reads=%d slept_ns=%ld rcvbuf=%d room_ns=%ld last_ns=%ld\n",
            0xa001 + i, t1 - t0, t0, first_end, second, got, reads, slept, rcvbuf, room, last);
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: server <file> <lock_ms>:<reads>[:<gap_ms>[:<body>]] ...\n");
        return 2;
    }
    path = argv[1];
    if (argc - 2 > MAX_PLAN) {
        fprintf(stderr, "server: a plan of %d requests is over the %d this build holds\n",
                argc - 2, MAX_PLAN);
        return 2;
    }
    for (int i = 2; i < argc && plan_n < MAX_PLAN; i++, plan_n++)
        if (sscanf(argv[i], "%ld:%ld:%ld:%ld", &plan_lock[plan_n], &plan_reads[plan_n],
                   &plan_gap[plan_n], &plan_body[plan_n]) < 2) return 2;

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
        /* The socket's cookie, as the kernel names it: what the tool's `sock`
         * key must be, read here by the server and nowhere else. */
        unsigned long long cookie = 0;
        socklen_t clen = sizeof cookie;
        getsockopt(conn, SOL_SOCKET, SO_COOKIE, &cookie, &clen);
        char buf[512];
        long got = 0, r;
        while (got < (long)sizeof buf - 1 && (r = recv(conn, buf + got, sizeof buf - 1 - got, 0)) > 0) {
            got += r;
            buf[got] = 0;
            if (strstr(buf, "\r\n\r\n")) break;
        }
        long lock_ms = 0, reads = 0, body = 0;
        sscanf(buf, "GET /?lock=%ld&reads=%ld&body=%ld", &lock_ms, &reads, &body);
        unsigned long id = 0xa001 + i;
        if (body != 0) {
            int want = SLOW_SNDBUF;
            setsockopt(conn, SOL_SOCKET, SO_SNDBUF, &want, sizeof want);
        }

        long born = now_ns();
        int guard = START_ENABLED();
        if (guard) DTRACE_PROBE2(omn, request__start, id, conn);

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

        char header[128];
        long length = body > 0 ? body : body < 0 ? -body : 2;
        int hn = snprintf(header, sizeof header,
                          "HTTP/1.1 200 OK\r\nContent-Length: %ld\r\nConnection: close\r\n\r\n",
                          length);
        char *response = malloc(hn + length);
        memcpy(response, header, hn);
        memset(response + hn, 'x', length);
        long send_start = now_ns(), sent = 0, w;
        while (sent < hn + length && (w = send(conn, response + sent, hn + length - sent, 0)) > 0)
            sent += w;
        long send_ns = now_ns() - send_start;
        free(response);
        if (!abandoned && DONE_ENABLED()) DTRACE_PROBE2(omn, request__done, id, conn);
        long ended = now_ns();
        close(conn);
        SAY("role=worker req=%lx tid=%d lock_ms=%ld reads=%ld done_reads=%ld lock_ns=%ld "
            "read_ns=%ld send_start_ns=%ld send_ns=%ld sent=%ld span_ns=%ld born_ns=%ld cookie=%llu "
            "guard=%d abandoned=%d\n",
            id, tid(), lock_ms, reads, done_reads, lock_ns, read_ns, send_start, send_ns, sent,
            ended - born, born, cookie, guard, abandoned);
    }
    pthread_join(c, NULL);
    uint64_t quit = QUIT;
    (void)!write(hold_fd, &quit, sizeof quit);
    pthread_join(h, NULL);
    unlink(path);
    return 0;
}
/* END SERVER */
