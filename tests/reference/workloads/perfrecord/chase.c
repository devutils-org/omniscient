/* A pointer chase over a buffer far larger than the last-level cache, so
 * most loads miss to memory and both perf mem and omn's mem.latency have a
 * distribution to read. Built and run by build.sh. */
#include <stdint.h>
#include <stdlib.h>
#include <time.h>

int main(int argc, char **argv)
{
    double seconds = argc > 1 ? atof(argv[1]) : 2.0;
    size_t n = (size_t)64 << 20 >> 3; /* 64 MiB of pointers */
    size_t *next = malloc(n * sizeof *next);
    if (!next)
        return 1;
    for (size_t i = 0; i < n; i++)
        next[i] = i;
    srand(42);
    for (size_t i = n - 1; i > 0; i--) { /* Sattolo: one cycle through all */
        size_t j = (((size_t)rand() << 31) ^ (size_t)rand()) % i;
        size_t t = next[i];
        next[i] = next[j];
        next[j] = t;
    }
    struct timespec start, now;
    clock_gettime(CLOCK_MONOTONIC, &start);
    volatile size_t p = 0;
    size_t q = 0;
    do {
        for (int k = 0; k < 100000; k++)
            q = next[q];
        p = q;
        clock_gettime(CLOCK_MONOTONIC, &now);
    } while ((now.tv_sec - start.tv_sec) + (now.tv_nsec - start.tv_nsec) / 1e9 < seconds);
    (void)p;
    return 0;
}
