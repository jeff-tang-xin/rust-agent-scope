/** The Services tree (Docker → Containers / Images / Volumes / Networks), built from a snapshot. */
import type { DockerContainer, DockerImage, DockerNetwork, DockerSnapshot, DockerVolume } from "@/platform/ipc";

export type GroupKind = "containers" | "images" | "volumes" | "networks";

export type ServiceNode =
  | { kind: "docker"; key: string }
  | { kind: "group"; key: string; group: GroupKind; count: number }
  | { kind: "compose"; key: string; project: string; containers: DockerContainer[] }
  | { kind: "container"; key: string; container: DockerContainer }
  | { kind: "image"; key: string; image: DockerImage }
  | { kind: "volume"; key: string; volume: DockerVolume }
  | { kind: "network"; key: string; network: DockerNetwork };

export const DOCKER_KEY = "docker";
const GROUPS: Array<[GroupKind, string]> = [
  ["containers", "Containers"],
  ["images", "Images"],
  ["volumes", "Volumes"],
  ["networks", "Networks"],
];

export const shortId = (id: string) => id.replace(/^sha256:/, "").slice(0, 12);

export function isRunning(c: DockerContainer): boolean {
  return c.state === "running" || c.state === "restarting" || c.state === "paused";
}

export function imageRef(i: DockerImage): string {
  if (!i.repository || i.repository === "<none>") return shortId(i.id);
  return !i.tag || i.tag === "<none>" ? i.repository : `${i.repository}:${i.tag}`;
}

export function rootNodes(): ServiceNode[] {
  return [{ kind: "docker", key: DOCKER_KEY }];
}

/** Children lookup over one snapshot (stable node objects per snapshot). */
export function childrenOf(snap: DockerSnapshot | null): (n: ServiceNode) => ServiceNode[] | undefined {
  const cache = new Map<string, ServiceNode[] | undefined>();
  const compute = (n: ServiceNode): ServiceNode[] | undefined => {
    if (!snap || snap.availability.kind !== "ready") return undefined;
    switch (n.kind) {
      case "docker":
        return GROUPS.map(([group]) => ({
          kind: "group",
          key: `group:${group}`,
          group,
          count: groupCount(snap, group),
        }));
      case "group":
        return groupChildren(snap, n.group);
      case "compose":
        return n.containers.map(containerNode);
      default:
        return undefined;
    }
  };
  return (n) => {
    if (!cache.has(n.key)) cache.set(n.key, compute(n));
    return cache.get(n.key);
  };
}

function groupCount(snap: DockerSnapshot, g: GroupKind): number {
  return g === "containers"
    ? snap.containers.length
    : g === "images"
      ? snap.images.length
      : g === "volumes"
        ? snap.volumes.length
        : snap.networks.length;
}

const containerNode = (c: DockerContainer): ServiceNode => ({
  kind: "container",
  key: `container:${c.id}`,
  container: c,
});

/** Compose projects (by label) first, then standalone containers, as IDEA groups them. */
export function groupChildren(snap: DockerSnapshot, g: GroupKind): ServiceNode[] {
  switch (g) {
    case "containers": {
      const projects = new Map<string, DockerContainer[]>();
      const loose: DockerContainer[] = [];
      for (const c of snap.containers) {
        if (c.composeProject) projects.set(c.composeProject, [...(projects.get(c.composeProject) ?? []), c]);
        else loose.push(c);
      }
      const compose: ServiceNode[] = [...projects.entries()]
        .sort((a, b) => a[0].localeCompare(b[0]))
        .map(([project, containers]) => ({
          kind: "compose",
          key: `compose:${project}`,
          project,
          containers: [...containers].sort((a, b) =>
            (a.composeService ?? a.name).localeCompare(b.composeService ?? b.name),
          ),
        }));
      return [...compose, ...loose.map(containerNode)];
    }
    case "images":
      return snap.images.map((image) => ({ kind: "image", key: `image:${image.id}`, image }));
    case "volumes":
      return snap.volumes.map((volume) => ({ kind: "volume", key: `volume:${volume.name}`, volume }));
    case "networks":
      return snap.networks.map((network) => ({ kind: "network", key: `network:${network.id}`, network }));
  }
}

export function nodeLabel(n: ServiceNode): string {
  switch (n.kind) {
    case "docker":
      return "Docker";
    case "group":
      return GROUPS.find(([g]) => g === n.group)![1];
    case "compose":
      return n.project;
    case "container":
      return n.container.composeService && n.container.composeProject ? n.container.composeService : n.container.name;
    case "image":
      return imageRef(n.image);
    case "volume":
      return n.volume.name;
    case "network":
      return n.network.name;
  }
}

export function nodeDetail(n: ServiceNode): string | undefined {
  switch (n.kind) {
    case "group":
      return n.count ? String(n.count) : undefined;
    case "compose": {
      const up = n.containers.filter(isRunning).length;
      return `${up}/${n.containers.length} running`;
    }
    case "container":
      return `${n.container.status}${n.container.composeService ? ` · ${n.container.name}` : ` · ${n.container.image}`}`;
    case "image":
      return `${n.image.size} · ${n.image.createdSince}`;
    case "volume":
      return n.volume.driver;
    case "network":
      return n.network.driver;
    default:
      return undefined;
  }
}

export function nodeIcon(n: ServiceNode): string {
  switch (n.kind) {
    case "docker":
      return "file-docker";
    case "group":
      return "folder";
    case "compose":
      return "file-yaml";
    case "container":
      return n.container.state === "paused" ? "pause" : isRunning(n.container) ? "run" : "module";
    case "image":
      return "package";
    case "volume":
      return "database";
    case "network":
      return "hierarchy";
  }
}

/** The containers a node stands for (a compose project's, or one). */
export function nodeContainers(n: ServiceNode): DockerContainer[] {
  return n.kind === "container" ? [n.container] : n.kind === "compose" ? n.containers : [];
}

/** Finds the node with `key` in a fresh snapshot (selection survives refreshes). */
export function findNode(snap: DockerSnapshot | null, key: string | null): ServiceNode | null {
  if (!snap || !key) return null;
  const children = childrenOf(snap);
  const walk = (nodes: readonly ServiceNode[]): ServiceNode | null => {
    for (const n of nodes) {
      if (n.key === key) return n;
      const found = walk(children(n) ?? []);
      if (found) return found;
    }
    return null;
  };
  return walk(rootNodes());
}

// ---------------------------------------------------------------------------
// Keys in the Services tree: every connection has its own subtree.

/** A node of one connection's subtree. */
export interface DockerTarget {
  conn: string;
  node: ServiceNode;
}

export const connectionKey = (conn: string) => `docker:${conn}`;
export const registryKey = (id: string) => `docker-registry:${id}`;

/** The tree key of `node` (whose key is local to its snapshot) under connection `conn`. */
export function treeKey(conn: string, node: ServiceNode): string {
  return node.kind === "docker" ? connectionKey(conn) : `${connectionKey(conn)}:${node.key}`;
}

/** Splits a tree key into connection id and local node key. */
export function parseTreeKey(key: string): { conn: string; local: string } | null {
  if (!key.startsWith("docker:")) return null;
  const rest = key.slice("docker:".length);
  const i = rest.indexOf(":");
  return i < 0 ? { conn: rest, local: DOCKER_KEY } : { conn: rest.slice(0, i), local: rest.slice(i + 1) };
}
