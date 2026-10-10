// A group of three
// consecutive line comments
// at the same indent.
export function grouped(): number {
  // An indented group,
  // two lines long.
  const x = 1;
  // Indent changes
    // split the group here.
  return x;
}

// Before the directive.
// eslint-disable-next-line no-console
// After the directive (adjacent, protected).
console.log("interrupted");

// A blank line ends a group.

// So this starts another.
const y = 2; // a trailing comment
// never joins a group.
export const z = y;
