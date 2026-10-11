import { formatCents } from "./money.js";

// Tax is applied per line rather than once on the subtotal: the regional
// rules we follow round each taxable line separately, and rounding the
// subtotal instead drifts by a cent on large carts.
const TAX_RATE_BASIS_POINTS = 825;

/** Adds an item to the cart, merging quantities when the SKU is already present. */
export function addItem(cart, item) {
  // Find the existing line.
  const existing = cart.lines.find((line) => line.sku === item.sku);
  if (existing) {
    // make sure we dont double count stuff when the same sku comes in twice
    existing.quantity += item.quantity;
    return cart;
  }
  // Add the item to the lines.
  cart.lines.push({ ...item });
  return cart;
}

/** Computes the tax for one line. */
function lineTax(line) {
  // Math.round rounds half up, which is what the tax authority publishes;
  // a banker's-rounding helper would under-collect on exact half cents.
  return Math.round((line.unitCents * line.quantity * TAX_RATE_BASIS_POINTS) / 10000);
}

export function cartTotal(cart) {
  let total = 0; // initialize total to zero
  for (const line of cart.lines) {
    // loop over the lines and add each line total plus its tax
    total += line.unitCents * line.quantity + lineTax(line);
  }
  return formatCents(total);
}

// TODO: support coupon codes once the pricing service exposes them.
export const EMPTY_CART = Object.freeze({ lines: [] });
