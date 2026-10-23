#ifndef LLVBF_INTTYPES_H
#define LLVBF_INTTYPES_H
#include <stdint.h>
#define PRId8 "d"
#define PRIi8 "d"
#define PRIu8 "u"
#define PRIx8 "x"
#define PRId16 "d"
#define PRIi16 "d"
#define PRIu16 "u"
#define PRIx16 "x"
#if __INT_WIDTH__ >= 32
#define PRId32 "d"
#define PRIi32 "d"
#define PRIu32 "u"
#define PRIx32 "x"
#define PRIX32 "X"
#else
#define PRId32 "ld"
#define PRIi32 "ld"
#define PRIu32 "lu"
#define PRIx32 "lx"
#define PRIX32 "lX"
#endif
#define PRId64 "lld"
#define PRIi64 "lld"
#define PRIu64 "llu"
#define PRIx64 "llx"
#endif
