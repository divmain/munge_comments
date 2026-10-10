export const port = 8080;

// @ts-expect-error
/** The legacy alias is still accepted here. */
const legacy: string = port;

// eslint-disable-next-line no-var
// This explanation sits between the directive and its target.
var hoisted = legacy;

/* c8 ignore next 2 */
const skipped = hoisted.length;
// Line 2 of the c8 window: protected.
const counted = skipped + 1;
// Line 4, past the window: ordinary prose.
export const total = counted;
