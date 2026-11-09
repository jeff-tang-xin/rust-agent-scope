/**
 * Run targets of Dockerfiles (`FROM` lines → build image) and Compose files
 * (`services:` → up all, each service → up that service), detected per line like
 * the run gutter does (the configurations they start are in `runTypes.ts`).
 */
export type DockerTarget =
  | { kind: "build"; line: number; stage: string | null; last: boolean }
  | { kind: "composeAll"; line: number }
  | { kind: "composeService"; line: number; service: string };

const base = (path: string) => path.slice(path.lastIndexOf("/") + 1);
const dir = (path: string) => path.slice(0, Math.max(0, path.lastIndexOf("/"))) || "/";

export function isDockerfile(path: string): boolean {
  const name = base(path);
  return /^(Dockerfile|Containerfile)(\.[\w.-]+)?$/i.test(name) || /\.(dockerfile|containerfile)$/i.test(name);
}

export function isComposeFile(path: string): boolean {
  return /^(docker-)?compose(\.[\w-]+)?\.ya?ml$/i.test(base(path));
}

/** Every `FROM` line; a named stage (`AS build`) is built with `--target`, the last one whole. */
export function dockerfileTargets(text: string): DockerTarget[] {
  const out: Array<Extract<DockerTarget, { kind: "build" }>> = [];
  text.split("\n").forEach((line, i) => {
    const m = /^\s*FROM\s+(?:--\S+\s+)*\S+(?:\s+AS\s+([\w.-]+))?\s*$/i.exec(line.replace(/\r$/, ""));
    if (m) out.push({ kind: "build", line: i, stage: m[1] ?? null, last: false });
  });
  if (out.length) out[out.length - 1]!.last = true;
  return out;
}

const KEY = /^(\s*)("([^"]+)"|'([^']+)'|[A-Za-z0-9._-]+)\s*:\s*(#.*)?$/;

/** The top-level `services:` key and each service directly below it. */
export function composeTargets(text: string): DockerTarget[] {
  const lines = text.split("\n").map((l) => l.replace(/\r$/, ""));
  const out: DockerTarget[] = [];
  const start = lines.findIndex((l) => /^services\s*:\s*(#.*)?$/.test(l));
  if (start < 0) return out;
  out.push({ kind: "composeAll", line: start });
  let indent: number | null = null;
  for (let i = start + 1; i < lines.length; i++) {
    const line = lines[i]!;
    if (!line.trim() || line.trimStart().startsWith("#")) continue;
    const lead = line.length - line.trimStart().length;
    if (lead === 0) break; // next top-level key
    indent ??= lead;
    if (lead !== indent) continue;
    const m = KEY.exec(line);
    if (m) out.push({ kind: "composeService", line: i, service: m[3] ?? m[4] ?? m[2]! });
  }
  return out;
}

export function fileTargets(path: string, text: string): DockerTarget[] {
  if (isDockerfile(path)) return dockerfileTargets(text);
  if (isComposeFile(path)) return composeTargets(text);
  return [];
}

/** An image tag from the build context's folder name (`My App` → `my-app:latest`). */
export function imageTag(path: string): string {
  const folder = base(dir(path))
    .toLowerCase()
    .replace(/[^a-z0-9._-]+/g, "-")
    .replace(/^[-._]+|[-._]+$/g, "");
  return `${folder || "image"}:latest`;
}

export function targetTitle(t: DockerTarget, path: string): string {
  switch (t.kind) {
    case "build":
      return t.stage && !t.last ? `Build Stage '${t.stage}'` : `Run '${base(path)}'`;
    case "composeAll":
      return "Compose Up (All Services)";
    case "composeService":
      return `Compose Up '${t.service}'`;
  }
}
