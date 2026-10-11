//
export function Label(props: { text: string }) {
  /* */
  const text = props.text.trim(); //
  return <span>{text}</span>;
}
