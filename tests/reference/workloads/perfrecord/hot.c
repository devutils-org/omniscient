/* Three leaf functions whose shares of the run are set by construction:
 * every round calls each for a fixed number of iterations of the same loop,
 * 20, 50 and 30 per cent of the work. Built and run by build.sh.
 *
 * A round's length varies (the three counts are scaled by one pseudo-random
 * factor from 1 to 8, which keeps the shares): a round of fixed length lasts
 * about a millisecond here, close to a 997 Hz sampler's period, and a
 * periodic sampler then walks slowly through the round's phases and lands in
 * one function more than its share. Seen before the change: one function's
 * share read 0.46 to 0.57 from run to run, on perf's cpu-clock as on omn's. */
#include <stdint.h>
#include <stdlib.h>
#include <time.h>

static volatile uint64_t sink;

#define SPIN(name)                                                   \
    __attribute__((noinline)) static void name(uint64_t n)           \
    {                                                                \
        uint64_t x = n;                                              \
        for (uint64_t i = 0; i < n; i++)                             \
            x = x * 6364136223846793005ULL + 1442695040888963407ULL; \
        sink = x;                                                    \
    }

SPIN(hot_alpha)
SPIN(hot_beta)
SPIN(hot_gamma)

int main(int argc, char **argv)
{
    double seconds = argc > 1 ? atof(argv[1]) : 2.0;
    struct timespec start, now;
    uint64_t seed = 88172645463325252ULL;
    clock_gettime(CLOCK_MONOTONIC, &start);
    do {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        uint64_t scale = 1 + seed % 8;
        hot_alpha(20000 * scale);
        hot_beta(50000 * scale);
        hot_gamma(30000 * scale);
        clock_gettime(CLOCK_MONOTONIC, &now);
    } while ((now.tv_sec - start.tv_sec) + (now.tv_nsec - start.tv_nsec) / 1e9 < seconds);
    return 0;
}
