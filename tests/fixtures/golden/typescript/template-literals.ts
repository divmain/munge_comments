const name = "world";

// A template with a comment-looking literal part.
const plain = `// not a comment ${name} /* nor this */`;

const nested = `outer ${`inner // text ${name /* a real comment */}`} tail`;

const multiline = `
  // still template text
  ${/* a comment inside a substitution */ name}
`;
