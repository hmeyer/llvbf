#include <stdio.h>

typedef int (*binop)(int, int);

static int add(int a, int b) { return a + b; }
static int sub(int a, int b) { return a - b; }
static int mul(int a, int b) { return a * b; }
static int divi(int a, int b) { return b ? a / b : 0; }
static int mod(int a, int b) { return b ? a % b : 0; }

static binop lookup(char op) {
    switch (op) {
    case '+': return add;
    case '-': return sub;
    case '*': return mul;
    case '/': return divi;
    case '%': return mod;
    default: return NULL;
    }
}

int main(void) {
    const char *ops = "+-*/%?";
    for (const char *p = ops; *p; p++) {
        binop f = lookup(*p);
        if (f) printf("17 %c 5 = %d\n", *p, f(17, 5));
        else printf("unknown operator %c\n", *p);
    }
    printf("-17 / 5 = %d, -17 %% 5 = %d\n", divi(-17, 5), mod(-17, 5));
    return 0;
}
