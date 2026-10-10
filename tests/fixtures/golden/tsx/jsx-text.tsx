// JSX text that looks like comments is not a comment.
export function Banner() {
  return (
    <div>
      <p>// not a comment</p>
      // also not a comment, own line
      <span>/* still text */</span>
    </div>
  );
}
