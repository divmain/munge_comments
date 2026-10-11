/**
 * Formats an amount of integer cents as a dollar string.
 * @param {number} cents
 * @returns {string}
 */
export function formatCents(cents) {
  // Every amount in the web client is integer cents (cart.js never hands
  // us floats), so the only division happens here, at display time.
  const dollars = Math.trunc(cents / 100);
  // Get the remaining cents.
  const rest = Math.abs(cents % 100);
  // Pad the cents to two digits.
  return `${dollars}.${String(rest).padStart(2, "0")}`;
}

// eslint-disable-next-line no-restricted-globals
// The legacy checkout page still reads this global until it is migrated.
self.formatCents = formatCents;

//
export const CURRENCY = "USD";
