/** Placeholder rows shown while a table's data is on its way. */
export function LoadingTable({ columns, rows = 4 }: { columns: number; rows?: number }) {
  return (
    <div className="overflow-x-auto" aria-busy="true">
      <table className="w-full border-collapse text-left text-[13px]">
        <tbody>
          {Array.from({ length: rows }, (_, rowIndex) => (
            <tr key={rowIndex} className="border-t border-line first:border-t-0">
              {Array.from({ length: columns }, (_, cellIndex) => (
                <td key={cellIndex} className="px-4 py-3.5">
                  <span className="block h-3.5 w-full max-w-40 animate-pulse rounded bg-quiet-soft" />
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
