#include <stdio.h>

int main(void) {
    long a = 123456789L;
    long b = 1000L;
    printf("%ld * 3 = %ld\n", a, a * 3);
    printf("%ld / %ld = %ld\n", a, b, a / b);
    printf("%ld %% %ld = %ld\n", a, b, a % b);
    printf("-%ld / 7 = %ld\n", a, -a / 7);
    unsigned long u = 0xDEADBEEFUL;
    printf("%lx >> 4 = %lx, << 4 = %lx\n", u, u >> 4, u << 4);
    printf("%lu & 0xFFFF = %lu\n", u, u & 0xFFFFUL);
    unsigned x = 60000u;
    printf("%u + 10000 = %u (unsigned int has %d bits)\n", x, x + 10000u, (int)(sizeof(unsigned) * 8));
    int neg = -12345;
    printf("%d %x %o\n", neg, (unsigned)neg, (unsigned)neg);
    long long big = 1LL << 40;
    printf("1<<40 has %d bits\n", (int)(sizeof(big) * 8));
    printf("%d\n", (int)(big / 1099511627776LL));
    return 0;
}
