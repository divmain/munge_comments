#include <stddef.h>

/* Capacity must stay a power of two: the index math below masks with
   (RING_CAPACITY - 1) instead of using a modulo. */
#define RING_CAPACITY 64

/* The ring buffer struct. */
struct ring {
    unsigned char data[RING_CAPACITY];
    /* head index */
    size_t head;
    size_t tail;
};

/* Push a byte. */
int ring_push(struct ring *r, unsigned char byte) {
    /* Full when the free-running indices are exactly one capacity apart;
       head and tail are never reset, so this also works after wraparound. */
    if (r->head - r->tail == RING_CAPACITY) {
        return -1;
    }
    /* Store the byte. */
    r->data[r->head & (RING_CAPACITY - 1)] = byte;
    /* Increment head. */
    r->head++;
    return 0;
}

int ring_pop(struct ring *r, unsigned char *out) {
    if (r->head == r->tail) {
        return -1;
    }
    *out = r->data[r->tail & (RING_CAPACITY - 1)];
    r->tail++;
    return 0;
}
