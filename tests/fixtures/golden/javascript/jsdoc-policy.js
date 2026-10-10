/** Internal helper: prose. */
function internal(x) {
  return x + 1;
}

/** Exported: doc-protected. */
export function exported(x) {
  return internal(x);
}

/**
 * Load-bearing under checkJs: functional.
 * @param {number} x
 * @returns {number}
 */
function typed(x) {
  return x * 2;
}

/** @typedef {{ id: number }} Item */

/** @deprecated Use exported() instead. */
export function legacy(x) {
  return typed(x);
}

/* A plain block on an export is not JSDoc: prose. */
export const plain = 1;
