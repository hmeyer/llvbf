#include <stdio.h>
#include <string.h>

#define N 200

int main(void) {
    static char composite[N + 1];
    memset(composite, 0, sizeof composite);
    int count = 0;
    for (int i = 2; i <= N; i++) {
        if (composite[i]) continue;
        count++;
        printf("%d ", i);
        for (int j = i * i; j <= N; j += i) composite[j] = 1;
    }
    printf("\n%d primes up to %d\n", count, N);
    return 0;
}
