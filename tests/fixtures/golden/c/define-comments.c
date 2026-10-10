#define LINE_COMMENTED 1 // swallowed into the macro body, not inventoried
#define BLOCK_COMMENTED 2 /* a trailing block comment: inventoried */

/* Uses both macros. */
int sum(void) {
    return LINE_COMMENTED + BLOCK_COMMENTED;
}
