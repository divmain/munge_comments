#include <stdint.h>

#define FRAME_MAGIC 0xB1 /* magic byte */

/* Returns the header length for a frame type. */
int header_length(int type) {
    int length = 0;
    switch (type) {
    case 2:
        /* extended frames carry a 4 byte sequence number in front of the normal header */
        length += 4;
        /* FALLTHROUGH */
    case 1:
        length += 2;
        break;
    default:
        /* Unknown types are treated as headerless so old readers can skip
           frames added by newer firmware instead of rejecting the stream. */
        break;
    }
    return length;
}

/* Check the magic byte. */
int is_frame(const uint8_t *bytes) {
    /* Compare the first byte with the magic. */
    return bytes[0] == FRAME_MAGIC;
}
