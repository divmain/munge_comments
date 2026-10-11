import { LATE_FEE_BASIS_POINTS, rateCardFor } from "./rates";

/** An invoice line. */
export interface Line {
  plan: string;
  units: number;
}

// Calculate the subtotal.
export function subtotal(lines: Line[]): number {
  // Start at zero.
  let sum = 0;
  // Iterate over each line.
  for (const line of lines) {
    sum += rateCardFor(line.plan).unitCents * line.units;
  }
  return sum;
}

/**
 * Late fee for an overdue invoice.
 */
export function lateFee(cents: number, daysOverdue: number): number {
  // The grace period is contractual: customers were promised three days,
  // so a fee on day one or two generates a support ticket, not revenue.
  if (daysOverdue <= 3) {
    return 0;
  }
  // divide by 10000 bc its bps
  return Math.floor((cents * LATE_FEE_BASIS_POINTS) / 10_000);
}

// @ts-expect-error
// The legacy importer still passes the plan as a number.
export const legacyPlan: string = 42;

/* Check if the invoice is empty. */
export function isEmpty(lines: Line[]): boolean {
  return lines.length === 0;
}
