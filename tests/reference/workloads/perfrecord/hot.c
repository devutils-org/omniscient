/* Three leaf functions whose shares of the run are set by construction:
 * every round calls each for a fixed number of iterations of the same loop,
 * 20, 50 and 30 per cent of the work. Built and run by build.sh. */
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
    clock_gettime(CLOCK_MONOTONIC, &start);
    do {
        hot_alpha(200000);
        hot_beta(500000);
        hot_gamma(300000);
        clock_gettime(CLOCK_MONOTONIC, &now);
    } while ((now.tv_sec - start.tv_sec) + (now.tv_nsec - start.tv_nsec) / 1e9 < seconds);
    return 0;
}
