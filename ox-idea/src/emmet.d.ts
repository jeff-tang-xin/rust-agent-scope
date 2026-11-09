// `emmet` ships typings its package.json "exports" does not expose; the subset used here.
declare module "emmet" {
  export type SyntaxType = "markup" | "stylesheet";
  export interface UserConfig {
    type?: SyntaxType;
    syntax?: string;
    options?: Record<string, unknown>;
  }
  export interface ExtractedAbbreviation {
    abbreviation: string;
    location: number;
    start: number;
    end: number;
  }
  export default function expandAbbreviation(abbr: string, config?: UserConfig): string;
  export function extract(
    line: string,
    pos?: number,
    options?: Partial<{ lookAhead: boolean; type: SyntaxType; prefix: string }>,
  ): ExtractedAbbreviation | undefined;
}
