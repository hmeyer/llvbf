/* Minimal stdio for llvbf: everything goes to the single Brainfuck output stream. */
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <llvbf.h>

static FILE __files[3];
FILE *stdin = &__files[0];
FILE *stdout = &__files[1];
FILE *stderr = &__files[2];
int errno;

int putchar(int c) {
    __llvbf_putc(c);
    return (unsigned char)c;
}

int getchar(void) {
    int c = __llvbf_getc();
    /* Brainfuck interpreters report end of input as an unchanged (0) or -1 cell. */
    if (c == 0 || c == 255) return EOF;
    return c;
}

int fgetc(FILE *f) {
    (void)f;
    return getchar();
}

char *fgets(char *s, int n, FILE *f) {
    (void)f;
    int i = 0;
    while (i < n - 1) {
        int c = getchar();
        if (c == EOF) break;
        s[i++] = (char)c;
        if (c == '\n') break;
    }
    if (i == 0) return 0;
    s[i] = 0;
    return s;
}

int puts(const char *s) {
    while (*s) __llvbf_putc(*s++);
    __llvbf_putc('\n');
    return 0;
}

int fputc(int c, FILE *f) {
    (void)f;
    return putchar(c);
}

int fputs(const char *s, FILE *f) {
    (void)f;
    while (*s) __llvbf_putc(*s++);
    return 0;
}

int fflush(FILE *f) {
    (void)f;
    return 0;
}

/* ---- formatted output ---- */

struct out {
    char *buf;      /* NULL: write to the output stream */
    size_t cap;     /* capacity of buf including the terminator */
    size_t len;     /* characters that would have been written */
};

static void out_char(struct out *o, char c) {
    if (o->buf) {
        if (o->len + 1 < o->cap) o->buf[o->len] = c;
    } else {
        __llvbf_putc(c);
    }
    o->len++;
}

static void out_pad(struct out *o, int n, char c) {
    while (n-- > 0) out_char(o, c);
}

static void out_str(struct out *o, const char *s, int prec) {
    int n = 0;
    while (s[n] && (prec < 0 || n < prec)) n++;
    for (int i = 0; i < n; i++) out_char(o, s[i]);
}

static int format_uint(char *tmp, unsigned long v, unsigned base, int upper) {
    const char *digits = upper ? "0123456789ABCDEF" : "0123456789abcdef";
    int n = 0;
    if (v == 0) tmp[n++] = '0';
    while (v) {
        tmp[n++] = digits[v % base];
        v /= base;
    }
    return n; /* reversed */
}

static void emit_number(struct out *o, const char *tmp, int n, int neg, int plus, int width, int zero, int left, int prec, const char *prefix) {
    int digits = n > prec ? n : prec;
    int sign = neg ? 1 : plus ? 1 : 0;
    int plen = (int)strlen(prefix);
    int total = digits + sign + plen;
    if (!left && !zero) out_pad(o, width - total, ' ');
    if (neg) out_char(o, '-');
    else if (plus) out_char(o, '+');
    out_str(o, prefix, -1);
    if (!left && zero) out_pad(o, width - total, '0');
    out_pad(o, digits - n, '0');
    for (int i = n - 1; i >= 0; i--) out_char(o, tmp[i]);
    if (left) out_pad(o, width - total, ' ');
}

static int vformat(struct out *o, const char *fmt, va_list ap) {
    char tmp[34];
    while (*fmt) {
        char c = *fmt++;
        if (c != '%') {
            out_char(o, c);
            continue;
        }
        int left = 0, plus = 0, zero = 0, alt = 0, space = 0;
        for (;;) {
            if (*fmt == '-') left = 1;
            else if (*fmt == '+') plus = 1;
            else if (*fmt == '0') zero = 1;
            else if (*fmt == '#') alt = 1;
            else if (*fmt == ' ') space = 1;
            else break;
            fmt++;
        }
        (void)space;
        int width = 0;
        if (*fmt == '*') {
            width = va_arg(ap, int);
            if (width < 0) { left = 1; width = -width; }
            fmt++;
        } else {
            while (*fmt >= '0' && *fmt <= '9') width = width * 10 + (*fmt++ - '0');
        }
        int prec = -1;
        if (*fmt == '.') {
            fmt++;
            prec = 0;
            if (*fmt == '*') {
                prec = va_arg(ap, int);
                fmt++;
            } else {
                while (*fmt >= '0' && *fmt <= '9') prec = prec * 10 + (*fmt++ - '0');
            }
        }
        int longs = 0, shorts = 0;
        while (*fmt == 'l' || *fmt == 'h' || *fmt == 'z' || *fmt == 'j' || *fmt == 't') {
            if (*fmt == 'l') longs++;
            else if (*fmt == 'h') shorts++;
            fmt++;
        }
        char conv = *fmt++;
        if (!conv) break;
        switch (conv) {
        case 'd':
        case 'i': {
            long v = longs ? va_arg(ap, long) : va_arg(ap, int);
            if (shorts == 2) v = (signed char)v;
            int neg = v < 0;
            unsigned long u = neg ? -(unsigned long)v : (unsigned long)v;
            int n = format_uint(tmp, u, 10, 0);
            emit_number(o, tmp, n, neg, plus, width, zero && prec < 0, left, prec, "");
            break;
        }
        case 'u':
        case 'x':
        case 'X':
        case 'o': {
            unsigned long v = longs ? va_arg(ap, unsigned long) : va_arg(ap, unsigned);
            if (shorts == 2) v = (unsigned char)v;
            unsigned base = conv == 'u' ? 10 : conv == 'o' ? 8 : 16;
            int n = format_uint(tmp, v, base, conv == 'X');
            const char *prefix = "";
            if (alt && v) prefix = conv == 'x' ? "0x" : conv == 'X' ? "0X" : conv == 'o' ? "0" : "";
            emit_number(o, tmp, n, 0, 0, width, zero && prec < 0, left, prec, prefix);
            break;
        }
        case 'c': {
            char ch = (char)va_arg(ap, int);
            if (!left) out_pad(o, width - 1, ' ');
            out_char(o, ch);
            if (left) out_pad(o, width - 1, ' ');
            break;
        }
        case 's': {
            const char *s = va_arg(ap, const char *);
            if (!s) s = "(null)";
            int n = 0;
            while (s[n] && (prec < 0 || n < prec)) n++;
            if (!left) out_pad(o, width - n, ' ');
            out_str(o, s, n);
            if (left) out_pad(o, width - n, ' ');
            break;
        }
        case 'p': {
            unsigned long v = (unsigned long)(uintptr_t)va_arg(ap, void *);
            int n = format_uint(tmp, v, 16, 0);
            emit_number(o, tmp, n, 0, 0, width, 0, left, 4, "0x");
            break;
        }
        case '%':
            out_char(o, '%');
            break;
        default:
            out_char(o, '%');
            out_char(o, conv);
            break;
        }
    }
    return (int)o->len;
}

int vprintf(const char *fmt, va_list ap) {
    struct out o = {0, 0, 0};
    return vformat(&o, fmt, ap);
}

int printf(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vprintf(fmt, ap);
    va_end(ap);
    return r;
}

int fprintf(FILE *f, const char *fmt, ...) {
    (void)f;
    va_list ap;
    va_start(ap, fmt);
    int r = vprintf(fmt, ap);
    va_end(ap);
    return r;
}

int vsnprintf(char *buf, size_t n, const char *fmt, va_list ap) {
    struct out o = {buf, n, 0};
    int r = vformat(&o, fmt, ap);
    if (n > 0) buf[o.len < n ? o.len : n - 1] = 0;
    return r;
}

int snprintf(char *buf, size_t n, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, n, fmt, ap);
    va_end(ap);
    return r;
}

int sprintf(char *buf, const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    int r = vsnprintf(buf, (size_t)-1, fmt, ap);
    va_end(ap);
    return r;
}

void __llvbf_assert_fail(const char *expr, const char *file, int line) {
    printf("Assertion failed: %s (%s:%d)\n", expr, file, line);
    __llvbf_exit();
    for (;;) {}
}
