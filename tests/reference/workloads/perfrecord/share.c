/* False sharing by construction: two threads each increment their own
 * counter, and the two counters sit in one 64-byte line of a static array.
 * Built without PIE so the line's address is the same on every run, and
 * perf c2c and omn's mem.sharing can name it alike. Built and run by
 * build.sh. */
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <time.h>

static struct {
    volatile uint64_t a;
    volatile uint64_t b;
} __attribute__((aligned(64))) line;

static double seconds;

static double elapsed(const struct timespec *start)
{
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (now.tv_sec - start->tv_sec) + (now.tv_nsec - start->tv_nsec) / 1e9;
}

static void *writer(void *arg)
{
    volatile uint64_t *counter = arg;
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    do {
        for (int i = 0; i < 1000000; i++)
            (*counter)++;
    } while (elapsed(&start) < seconds);
    return NULL;
}

int main(int argc, char **argv)
{
    seconds = argc > 1 ? atof(argv[1]) : 2.0;
    pthread_t t1, t2;
    pthread_create(&t1, NULL, writer, (void *)&line.a);
    pthread_create(&t2, NULL, writer, (void *)&line.b);
    pthread_join(t1, NULL);
    pthread_join(t2, NULL);
    return 0;
}
