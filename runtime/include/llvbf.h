/* Compiler builtins of llvbf. These are lowered inline by the backend. */
#ifndef LLVBF_H
#define LLVBF_H

/* Write the low byte of c to the output. */
void __llvbf_putc(int c);
/* Read one byte from the input. Returns 0 at end of input on most interpreters
   (some return 255). */
int __llvbf_getc(void);
/* Stop the program. */
void __llvbf_exit(void);

/* First free byte of memory after the global data (defined by the compiler). */
extern char __llvbf_heap_start[];

#endif
