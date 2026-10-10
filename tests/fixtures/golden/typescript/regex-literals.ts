// Patterns used below.
const slashes = /\/\//g;
const block = /\/\*.*\*\//;
const klass = /[/]+/; // a slash inside a character class

export function strip(input: string): string {
  return input.replace(slashes, "/").replace(block, ""); /* trailing block */
}
