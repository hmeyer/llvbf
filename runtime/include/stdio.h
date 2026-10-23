#ifndef LLVBF_STDIO_H
#define LLVBF_STDIO_H
#include <stddef.h>
#include <stdarg.h>

#define EOF (-1)

typedef struct { int unused; } FILE;
extern FILE *stdin, *stdout, *stderr;

int putchar(int c);
int getchar(void);
int puts(const char *s);
int printf(const char *fmt, ...);
int vprintf(const char *fmt, va_list ap);
int sprintf(char *buf, const char *fmt, ...);
int snprintf(char *buf, size_t n, const char *fmt, ...);
int vsnprintf(char *buf, size_t n, const char *fmt, va_list ap);
int fputc(int c, FILE *f);
int fputs(const char *s, FILE *f);
int fprintf(FILE *f, const char *fmt, ...);
int fflush(FILE *f);
int fgetc(FILE *f);
char *fgets(char *s, int n, FILE *f);
#define putc(c, f) fputc(c, f)
#define getc(f) fgetc(f)

#endif
