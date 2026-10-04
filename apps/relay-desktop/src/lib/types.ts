export type RunnerState =
  | { kind: "notInitialized" }
  | { kind: "starting" }
  | { kind: "running" }
  | { kind: "paused" }
  | { kind: "error"; message: string }
  | { kind: "externalService"; message: string };

export interface Overview {
  initialized: boolean;
  deviceName: string | null;
  deviceId: string | null;
  suggestedName: string;
  runner: RunnerState;
  version: string;
  mobile: boolean;
  peerCount: number;
  connectedPeers: number;
  spaceCount: number;
  mountCount: number;
}

export interface PeerView {
  name: string;
  id: string;
  shortId: string;
  address: string;
  connected: boolean;
  /** Unix ms when the current session started. Set only while connected. */
  connectedSinceMs: number | null;
  /** Last live contact. Null until this peer has connected once. */
  lastSeenMs: number | null;
  /** This peer may browse this computer and set up sync on it. */
  allowedToManage: boolean;
  /** This peer lets this computer manage it. Known only while connected. */
  canManage: boolean;
  /** This peer's Relay answers remote calls. Known only while connected. */
  supportsRemote: boolean;
}

// Remote calls (D37). These are relay-core's shapes as-is, so their fields
// are snake_case like the rest of the IPC protocol.

export type RemoteCall =
  | { call: "roots" }
  | { call: "list_dir"; path: string; cursor?: number; limit?: number }
  | { call: "stat"; path: string }
  | { call: "spaces" };

export interface RemoteRoot {
  name: string;
  path: string;
}

export interface MountRef {
  space: string;
  mount: string;
}

export interface DirEntry {
  name: string;
  path: string;
  kind: "file" | "directory" | "symlink" | "other";
  size: number | null;
  modified_ms: number | null;
  hidden: boolean;
  /** A cloud placeholder: syncing it would download it. */
  cloud_only: boolean;
  mount: MountRef | null;
  contains_mount: boolean;
}

export interface DirListing {
  path: string;
  parent: string | null;
  entries: DirEntry[];
  next_cursor: number | null;
  total: number;
  inside_mount: MountRef | null;
}

export interface RemoteSpace {
  name: string;
  mounts: { name: string; path: string | null }[];
}

export type RemoteReply =
  | { reply: "roots"; roots: RemoteRoot[] }
  | { reply: "listing"; listing: DirListing }
  | { reply: "stat"; entry: DirEntry }
  | { reply: "spaces"; spaces: RemoteSpace[] };

export type RemoteErrorCode =
  | "forbidden"
  | "denied"
  | "not_found"
  | "timeout"
  | "unsupported"
  | "invalid"
  | "busy"
  | "offline"
  | "failed";

export interface PairStartResult {
  code: string;
  expiresAtMs: number;
}

export type PairStatus =
  | { state: "idle" }
  | { state: "waiting" }
  | { state: "paired"; peerName: string; peerId: string }
  | { state: "failed"; reason: string }
  | { state: "expired" };

export interface PairJoinResult {
  peerName: string;
  peerId: string;
}

export interface MountView {
  name: string;
  path: string | null;
  attached: boolean;
  state: string;
}

export interface SpaceView {
  name: string;
  id: string;
  mounts: MountView[];
  sharedWith: string[];
}

export interface OfferView {
  peer: string;
  peerId: string;
  name: string;
  mounts: { name: string }[];
}

export type ConflictClass =
  | { kind: "file"; original: string }
  | { kind: "git"; gitDir: string; isRef: boolean };

export interface DeleteHold {
  peer: string;
  peerName: string;
  space: string;
  spaceId: string;
  mount: string;
  mountId: string;
  deletions: number;
  live: number;
  heldAtMs: number;
  decision: "apply" | "restore" | null;
}

export interface ConflictView {
  path: string;
  space: string;
  mount: string;
  deviceId: string;
  deviceShort: string;
  deviceName: string | null;
  class: ConflictClass;
}

export interface ResolveReport {
  space: string;
  mount: string;
  copy: string;
  original: string;
  resolution: string;
  scanned: boolean;
}

export interface GitResolveReport {
  space: string;
  mount: string;
  gitDir: string;
  deleted: string[];
  kept: string[];
  scanned: boolean;
}

export interface TransferLive {
  peerId: string;
  peerName: string;
  space: string;
  mount: string | null;
  direction: "receive" | "send" | "index";
  filesDone: number;
  filesTotal: number | null;
  bytesDone: number;
  bytesTotal: number | null;
  bytesPerSec: number;
  startedAtMs: number;
  retries: number;
  currentPath: string | null;
}

export interface ActivityItem {
  tsMs: number;
  kind: string;
  message: string;
}

export interface Settings {
  startAtLogin: boolean;
  autoUpdate: boolean;
}

export interface UpdateInfo {
  opId: number;
  configured: boolean;
  available: boolean;
  version: string | null;
  notes: string | null;
  message: string;
  installing: boolean;
  restartAtMs: number | null;
  error: boolean;
}

export type UpdateProgress =
  | { kind: "checking"; opId: number }
  | { kind: "downloading"; opId: number; downloaded: number; total: number | null }
  | { kind: "installing"; opId: number }
  | { kind: "ready"; opId: number; version: string; restartAtMs: number }
  | { kind: "finished"; opId: number; message: string; error: boolean };

export interface UpdateAvailable {
  version: string;
  notes: string;
}

export type CliShell = "zsh" | "bash" | "fish";

export interface CliShellHint {
  shell: CliShell;
  configFile: string;
  snippet: string;
  hint: string;
  configured: boolean;
}

export interface CliStatus {
  sidecarPath: string | null;
  installPath: string | null;
  onPath: boolean;
  hint: string | null;
  detectedShell: CliShell | null;
  shellHints: CliShellHint[];
  pathConfigured: boolean;
}

export interface CliInstallResult {
  path: string;
  onPath: boolean;
  hint: string | null;
  message: string;
  detectedShell: CliShell | null;
  pathConfigured: boolean;
}

export type Page =
  | "overview"
  | "peers"
  | "spaces"
  | "browse"
  | "conflicts"
  | "activity"
  | "settings";
