/** Backend (LSP) symbol kinds → `SymbolBadge` kinds, so every kind gets an IDEA node icon. */
export function badgeKind(kind: string): string {
  switch (kind) {
    case "typeAlias":
    case "typeParameter":
      return "type";
    case "enumMember":
      return "constant";
    case "macro":
    case "operator":
      return "function";
    case "event":
    case "key":
      return "property";
    case "unknown":
      return "text";
    default:
      return kind;
  }
}
