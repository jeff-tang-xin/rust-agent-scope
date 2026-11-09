/** The IDE's frame bridge (`window.oxidea`), injected before the page's scripts. */
interface OxideaDocumentState {
  version: number;
  text: string;
}

interface OxideaTextEdit {
  from: number;
  to: number;
  insert: string;
}

interface OxideaFileFilter {
  name: string;
  extensions: string[];
}

interface OxideaBridge {
  postMessage(text: string): void;
  onMessage(fn: (text: string) => void): void;
  readonly theme: { dark: boolean; vars: Readonly<Record<string, string>> } | null;
  onThemeChange(fn: (theme: { dark: boolean; vars: Readonly<Record<string, string>> }) => void): void;
  document: {
    /** `name`: the file's name, without its directory. */
    get(): Promise<OxideaDocumentState & { readOnly: boolean; name: string }>;
    onChange(fn: (state: OxideaDocumentState) => void): void;
    edit(version: number, edits: OxideaTextEdit[]): Promise<{ version: number }>;
    replace(version: number, text: string): Promise<{ version: number }>;
  };
  saveFile(options: {
    name: string;
    data: string | Uint8Array | ArrayBuffer | Blob;
    filters?: OxideaFileFilter[];
  }): Promise<string | null>;
  openFile(options?: { filters?: OxideaFileFilter[] }): Promise<{ name: string; data: Uint8Array } | null>;
}

interface Window {
  oxidea?: OxideaBridge;
  EXCALIDRAW_ASSET_PATH?: string | string[];
}
