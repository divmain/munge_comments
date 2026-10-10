export function List({ items }: { items: string[] }) {
  return (
    <ul>
      {/* The sole content of a container. */}
      {
        // A line comment alone in a container.
      }
      {items.map((item) => (
        <li key={item}>{item /* beside an expression */}</li>
      ))}
    </ul>
  );
}
