import { subtotal, type Line } from "../src/billing/invoice";

// Props for the table.
interface InvoiceTableProps {
  lines: Line[];
  currency: string;
}

// Render the invoice table.
export function InvoiceTable({ lines, currency }: InvoiceTableProps) {
  // The key must be the index, not the plan: an invoice may bill the same
  // plan twice (a mid-cycle upgrade), and duplicate keys drop rows.
  const rows = lines.map((line, index) => (
    <tr key={index}>
      <td>{line.plan}</td>
      <td>{line.units}</td>
    </tr>
  ));
  return (
    <table>
      {/* Screen readers announce the caption before the rows, so it stays first. */}
      <caption>Invoice</caption>
      <tbody>{rows}</tbody>
      <tfoot>
        <tr>
          <td>{subtotal(lines)}</td>
          <td>{currency}</td>
        </tr>
      </tfoot>
    </table>
  );
}
