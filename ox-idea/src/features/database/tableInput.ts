import type { EditorInput } from "@/workbench/contributions";

export const TABLE_KIND = "db-table";

export interface TableRef {
  id: string;
  schema: string;
  table: string;
}

export function tableTitle(input: EditorInput): string {
  return (input.data as TableRef).table;
}

export function tableInput(id: string, schema: string, table: string): EditorInput {
  const data: TableRef = { id, schema, table };
  return { kind: TABLE_KIND, key: `${TABLE_KIND}:${id}:${schema}.${table}`, data };
}
