#include <stdio.h>
#include <stdlib.h>

struct node {
    int value;
    struct node *next;
};

static struct node *push(struct node *head, int v) {
    struct node *n = malloc(sizeof *n);
    n->value = v;
    n->next = head;
    return n;
}

static int sum(const struct node *n) {
    int s = 0;
    for (; n; n = n->next) s += n->value;
    return s;
}

int main(void) {
    struct node *head = NULL;
    for (int i = 1; i <= 10; i++) head = push(head, i * i);
    printf("sum of squares 1..10 = %d\n", sum(head));
    int count = 0;
    for (struct node *n = head; n; n = n->next) count++;
    printf("%d nodes\n", count);
    return 0;
}
