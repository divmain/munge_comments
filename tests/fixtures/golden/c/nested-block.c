/* C block comments do not nest: /* this opener is just text */
int a = 1;

int b = /* /* */ 2;

/*
 * A multi-line block with a stray /* inside.
 */
int c = 3;
