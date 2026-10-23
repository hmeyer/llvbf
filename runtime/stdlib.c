#include <stdlib.h>
#include <string.h>
#include <llvbf.h>

/* Bump allocator with a free list is overkill here: free() is a no-op. */
static char *heap_ptr;

void *malloc(size_t n) {
    if (!heap_ptr) heap_ptr = __llvbf_heap_start;
    n = (n + 1) & ~(size_t)1;
    char *p = heap_ptr;
    heap_ptr += n;
    return p;
}

void *calloc(size_t n, size_t m) {
    size_t total = n * m;
    void *p = malloc(total);
    if (p) memset(p, 0, total);
    return p;
}

void *realloc(void *p, size_t n) {
    void *q = malloc(n);
    if (p && q) memcpy(q, p, n); /* may over-read; the heap is contiguous */
    return q;
}

void free(void *p) {
    (void)p;
}

void exit(int code) {
    (void)code;
    __llvbf_exit();
    for (;;) {}
}

void abort(void) {
    __llvbf_exit();
    for (;;) {}
}

int abs(int x) {
    return x < 0 ? -x : x;
}

long labs(long x) {
    return x < 0 ? -x : x;
}

long strtol(const char *s, char **end, int base) {
    while (*s == ' ' || *s == '\t' || *s == '\n') s++;
    int neg = 0;
    if (*s == '-' || *s == '+') neg = *s++ == '-';
    if (base == 0) {
        if (s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) {
            base = 16;
            s += 2;
        } else if (s[0] == '0') {
            base = 8;
        } else {
            base = 10;
        }
    } else if (base == 16 && s[0] == '0' && (s[1] == 'x' || s[1] == 'X')) {
        s += 2;
    }
    long v = 0;
    for (;; s++) {
        int d;
        if (*s >= '0' && *s <= '9') d = *s - '0';
        else if (*s >= 'a' && *s <= 'z') d = *s - 'a' + 10;
        else if (*s >= 'A' && *s <= 'Z') d = *s - 'A' + 10;
        else break;
        if (d >= base) break;
        v = v * base + d;
    }
    if (end) *end = (char *)s;
    return neg ? -v : v;
}

int atoi(const char *s) {
    return (int)strtol(s, 0, 10);
}

long atol(const char *s) {
    return strtol(s, 0, 10);
}

static unsigned long rand_state = 1;

int rand(void) {
    rand_state = rand_state * 1103515245UL + 12345UL;
    return (int)((rand_state >> 16) & 0x7fff);
}

void srand(unsigned seed) {
    rand_state = seed;
}

void qsort(void *base, size_t n, size_t size, int (*cmp)(const void *, const void *)) {
    /* insertion sort: small, simple and good enough for Brainfuck */
    char *b = base;
    char tmp[64];
    for (size_t i = 1; i < n; i++) {
        size_t j = i;
        while (j > 0 && cmp(b + (j - 1) * size, b + j * size) > 0) {
            memcpy(tmp, b + j * size, size);
            memcpy(b + j * size, b + (j - 1) * size, size);
            memcpy(b + (j - 1) * size, tmp, size);
            j--;
        }
    }
}
