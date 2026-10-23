#include <string.h>
#include <stdlib.h>

void *memcpy(void *d, const void *s, size_t n) {
    char *dp = d;
    const char *sp = s;
    while (n--) *dp++ = *sp++;
    return d;
}

void *memmove(void *d, const void *s, size_t n) {
    char *dp = d;
    const char *sp = s;
    if (dp < sp) {
        while (n--) *dp++ = *sp++;
    } else {
        dp += n;
        sp += n;
        while (n--) *--dp = *--sp;
    }
    return d;
}

void *memset(void *d, int c, size_t n) {
    char *dp = d;
    while (n--) *dp++ = (char)c;
    return d;
}

int memcmp(const void *a, const void *b, size_t n) {
    const unsigned char *x = a, *y = b;
    while (n--) {
        if (*x != *y) return *x - *y;
        x++;
        y++;
    }
    return 0;
}

void *memchr(const void *s, int c, size_t n) {
    const unsigned char *p = s;
    while (n--) {
        if (*p == (unsigned char)c) return (void *)p;
        p++;
    }
    return 0;
}

size_t strlen(const char *s) {
    size_t n = 0;
    while (s[n]) n++;
    return n;
}

char *strcpy(char *d, const char *s) {
    char *r = d;
    while ((*d++ = *s++)) {}
    return r;
}

char *strncpy(char *d, const char *s, size_t n) {
    size_t i = 0;
    for (; i < n && s[i]; i++) d[i] = s[i];
    for (; i < n; i++) d[i] = 0;
    return d;
}

char *strcat(char *d, const char *s) {
    strcpy(d + strlen(d), s);
    return d;
}

char *strncat(char *d, const char *s, size_t n) {
    char *p = d + strlen(d);
    while (n-- && *s) *p++ = *s++;
    *p = 0;
    return d;
}

int strcmp(const char *a, const char *b) {
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return (unsigned char)*a - (unsigned char)*b;
}

int strncmp(const char *a, const char *b, size_t n) {
    while (n && *a && *a == *b) {
        a++;
        b++;
        n--;
    }
    if (n == 0) return 0;
    return (unsigned char)*a - (unsigned char)*b;
}

char *strchr(const char *s, int c) {
    for (;; s++) {
        if (*s == (char)c) return (char *)s;
        if (!*s) return 0;
    }
}

char *strrchr(const char *s, int c) {
    const char *r = 0;
    for (;; s++) {
        if (*s == (char)c) r = s;
        if (!*s) return (char *)r;
    }
}

char *strstr(const char *h, const char *n) {
    size_t ln = strlen(n);
    if (!ln) return (char *)h;
    for (; *h; h++) {
        if (*h == *n && strncmp(h, n, ln) == 0) return (char *)h;
    }
    return 0;
}

char *strdup(const char *s) {
    size_t n = strlen(s) + 1;
    char *d = malloc(n);
    if (d) memcpy(d, s, n);
    return d;
}
