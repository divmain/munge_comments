// Endpoints the client talks to.
const base = "http://example.com//api"; // trailing after a URL string
const single = 'not // a comment';
const blockish = "/* not a comment */";
const escaped = "quote \" // still a string";

/* A block comment after strings. */
export function url(path: string): string {
  return base + "//" + path; // concatenated slashes
}
