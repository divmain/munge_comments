// A generic arrow needs the trailing comma in .tsx.
export const identity = <T,>(x: T): T => x;

const pair = <A, B>(a: A, b: B): [A, B] => [a, b]; // two type parameters

/* Uses both helpers. */
export const both = () => pair(identity(1), identity("two"));
