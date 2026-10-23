#include <stdio.h>
#include <ctype.h>

/* Reads the input, reports line/word/char counts and echoes it uppercased. */
int main(void) {
    int c, lines = 0, words = 0, chars = 0, in_word = 0;
    while ((c = getchar()) != EOF) {
        chars++;
        if (c == '\n') lines++;
        if (isspace(c)) {
            in_word = 0;
        } else if (!in_word) {
            in_word = 1;
            words++;
        }
        putchar(toupper(c));
    }
    printf("%d lines, %d words, %d chars\n", lines, words, chars);
    return 0;
}
