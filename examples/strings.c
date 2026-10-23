#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <ctype.h>

static char *reverse(const char *s) {
    size_t n = strlen(s);
    char *r = malloc(n + 1);
    for (size_t i = 0; i < n; i++) r[i] = s[n - 1 - i];
    r[n] = 0;
    return r;
}

int main(void) {
    const char *words[] = {"brainfuck", "llvm", "backend"};
    char buf[64] = "";
    for (int i = 0; i < 3; i++) {
        char *r = reverse(words[i]);
        printf("%-10s -> %s (%d chars)\n", words[i], r, (int)strlen(r));
        strcat(buf, r);
        strcat(buf, " ");
        free(r);
    }
    for (char *p = buf; *p; p++) *p = toupper(*p);
    printf("[%s]\n", buf);
    printf("cmp: %d %d %d\n", strcmp("abc", "abd") < 0, strcmp("abc", "abc"), strcmp("b", "a") > 0);
    printf("%s\n", strstr("hello world", "o w"));
    return 0;
}
