export function empty(): void {
  // Nothing here yet.
}

export function last(): number {
  const x = 1;
  return x;
  // After the final statement, before the closing brace.
}

export const list = [
  1,
  2,
  // Before the closing bracket.
];

export const call = Math.max(
  1,
  /* before the closing paren */
);
