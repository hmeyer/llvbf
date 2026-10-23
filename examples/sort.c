#include <stdio.h>

int data[] = {42, 7, 19, -3, 88, 0, 15, 23, -17, 5};

static void bubble_sort(int *a, int n) {
    for (int i = 0; i < n - 1; i++) {
        for (int j = 0; j < n - 1 - i; j++) {
            if (a[j] > a[j + 1]) {
                int t = a[j];
                a[j] = a[j + 1];
                a[j + 1] = t;
            }
        }
    }
}

int main(void) {
    int n = sizeof(data) / sizeof(data[0]);
    bubble_sort(data, n);
    for (int i = 0; i < n; i++) {
        printf("%d%c", data[i], i == n - 1 ? '\n' : ' ');
    }
    return 0;
}
