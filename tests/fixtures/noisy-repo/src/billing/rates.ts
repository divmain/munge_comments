// Rates are stored in basis points (1/100 of a percent) because the
// finance export rejects fractional percentages; invoice.ts divides by
// 10_000 when it applies them.
export const LATE_FEE_BASIS_POINTS = 150;

// The discount rate.
export const LOYALTY_DISCOUNT_BASIS_POINTS = 500;

export interface RateCard {
  // the name
  name: string;
  /* Per-unit price in cents. */
  unitCents: number;
}

// Returns the rate card for a plan.
export function rateCardFor(plan: string): RateCard {
  // Enterprise prices are negotiated per contract and live in the CRM, not
  // here; callers must never bill an enterprise plan from this table.
  if (plan === "enterprise") {
    throw new Error("enterprise plans are billed from the CRM");
  }
  // return the card
  return { name: plan, unitCents: plan === "pro" ? 1200 : 500 };
}

// That's the end of the rate helpers.
