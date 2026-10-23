#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>

struct pair { int a; int b; };

static struct pair swap(struct pair p) {
    struct pair r = {p.b, p.a};
    return r;
}

static int ack(int m, int n) {
    if (m == 0) return n + 1;
    if (n == 0) return ack(m - 1, 1);
    return ack(m - 1, ack(m, n - 1));
}

static unsigned rotl16(unsigned x, int n) {
    return (x << n) | (x >> (16 - n));
}

static const char *classify(int c) {
    switch (c) {
    case 0: return "zero";
    case 1: case 2: case 3: return "small";
    case 10: return "ten";
    case 100: return "hundred";
    case -1: return "minus one";
    default: return "other";
    }
}

int main(void) {
    int r;
    printf("add_overflow: %d %d\n", __builtin_add_overflow(30000, 10000, &r), r);
    printf("sub_overflow: %d %d\n", __builtin_sub_overflow(-30000, 10000, &r), r);
    unsigned ur;
    printf("uadd_overflow: %d %u\n", __builtin_add_overflow(60000u, 10000u, &ur), ur);
    printf("umul_overflow: %d %u\n", __builtin_mul_overflow(300u, 300u, &ur), ur);
    printf("popcount(0x5555) = %d, clz(1) = %d, ctz(0x40) = %d\n", __builtin_popcount(0x5555), __builtin_clz(1), __builtin_ctz(0x40));
    printf("bswap16(0x1234) = %x\n", __builtin_bswap16(0x1234));
    printf("abs(-5) = %d, labs(-70000) = %ld\n", abs(-5), labs(-70000L));
    for (int n = 1; n <= 15; n += 7) printf("rotl16(0x8001, %d) = %x\n", n, rotl16(0x8001, n));
    struct pair p = {3, 4};
    p = swap(p);
    printf("swap: %d %d\n", p.a, p.b);
    printf("ack(2, 3) = %d\n", ack(2, 3));
    int keys[] = {0, 2, 10, 100, -1, 42};
    for (int i = 0; i < 6; i++) printf("%d is %s\n", keys[i], classify(keys[i]));
    int8_t s8 = -100;
    uint8_t u8 = 200;
    printf("i8: %d %d %d, u8: %u %u\n", s8 / 7, s8 % 7, (int)(s8 >> 2), u8 / 7, (unsigned)(u8 << 1) & 0xff);
    unsigned v = 0xBEEF;
    for (int k = 0; k < 16; k += 5) printf("0x%x >> %d = 0x%x, << %d = 0x%x\n", v, k, v >> k, k, (v << k) & 0xffff);
    printf("signed shifts: %d %d\n", -1000 >> 3, (-1000) / 8);
    printf("compare: %d %d %d %d\n", -1 < 1, (unsigned)-1 < 1u, 32767 > -32768, 200 > 100);
    long big = 2000000000L;
    printf("long: %ld %ld %ld\n", big + big, big / 3, big % 1000);
    return 0;
}
