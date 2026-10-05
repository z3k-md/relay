import { invoke } from "@tauri-apps/api/core";
import type {
  ActivityItem,
  CliInstallResult,
  CliShell,
  CliStatus,
  ConflictView,
  PairJoinResult,
  PairStartResult,
  PairStatus,
  GitResolveReport,
  DeleteHold,
  FolderPairParams,
  FolderPairPlan,
  FolderPairResult,
  FolderView,
  MountView,
  OfferView,
  Overview,
  PeerView,
  QuickOpen,
  RemoteCall,
  RemoteErrorCode,
  RemoteReply,
  ResolveReport,
  RunnerState,
  Settings,
  SpeedReport,
  SpaceView,
  UpdateAvailable,
  UpdateInfo,
} from "./types";

function asError(err: unknown): Error {
  if (typeof err === "string") {
    return new Error(err);
  }
  if (err instanceof Error) {
    return err;
  }
  return new Error(String(err));
}

/** What a thrown value says, for showing to the user. */
export function errorText(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/** A refused remote call, with the stable code from the other device. */
export class RemoteCallError extends Error {
  constructor(
    readonly code: RemoteErrorCode,
    message: string,
  ) {
    super(message);
  }
}

async function remoteCall(peer: string, request: RemoteCall): Promise<RemoteReply> {
  return remoteInvoke<RemoteReply>("remote_call", { peer, call: request });
}

/** A Tauri command whose errors are `RemoteError`s. */
async function remoteInvoke<T>(cmd: string, args: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (err) {
    if (err && typeof err === "object" && "code" in err && "message" in err) {
      const { code, message } = err as { code: RemoteErrorCode; message: string };
      throw new RemoteCallError(code, message);
    }
    throw asError(err);
  }
}

async function call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
  try {
    return await invoke<T>(cmd, args);
  } catch (err) {
    throw asError(err);
  }
}

export const api = {
  getOverview: () => call<Overview>("get_overview"),
  initDevice: (name: string) => call<Overview>("init_device", { name }),
  listPeers: () => call<PeerView[]>("list_peers"),
  addPeer: (name: string, deviceId: string, address: string) =>
    call<PeerView>("add_peer", { name, deviceId, address }),
  removePeer: (name: string) => call<void>("remove_peer", { name }),
  setPeerManage: (name: string, allowed: boolean) =>
    call<void>("set_peer_manage", { name, allowed }),
  remoteCall,
  /** About ten seconds: download, then upload (D48). */
  speedTest: (peer: string) => remoteInvoke<SpeedReport>("speed_test", { peer }),
  pairStart: (share: string[], allowManage: boolean) =>
    call<PairStartResult>("pair_start", { share, allowManage }),
  pairStatus: () => call<PairStatus>("pair_status"),
  pairJoin: (code: string, addr: string | undefined, allowManage: boolean) =>
    call<PairJoinResult>("pair_join", { code, addr: addr || null, allowManage }),
  pairCancel: () => call<void>("pair_cancel"),
  listSpaces: () => call<SpaceView[]>("list_spaces"),
  createSpace: (name: string) => call<SpaceView>("create_space", { name }),
  addMount: (space: string, mount: string, path: string) =>
    call<MountView>("add_mount", { space, mount, path }),
  removeMount: (space: string, mount: string) => call<void>("remove_mount", { space, mount }),
  share: (space: string, peer: string) => call<void>("share", { space, peer }),
  unshare: (space: string, peer: string) => call<void>("unshare", { space, peer }),
  listOffers: () => call<OfferView[]>("list_offers"),
  listFolder: (space: string, mount: string, path: string) =>
    call<FolderView>("list_folder", { space, mount, path }),
  downloadFile: (space: string, mount: string, path: string) =>
    call<void>("download_file", { space, mount, path }),
  openFile: (space: string, mount: string, path: string) =>
    call<void>("open_file", { space, mount, path }),
  freeUpSpace: (space: string, mount: string, path: string) =>
    call<number>("free_up_space", { space, mount, path }),
  setFolderMode: (space: string, mount: string, path: string, mode: string | null) =>
    call<void>("set_folder_mode", { space, mount, path, mode }),
  folderPairPreview: (params: FolderPairParams) =>
    call<FolderPairPlan>("folder_pair_preview", { params }),
  folderPair: (params: FolderPairParams) => call<FolderPairResult>("folder_pair", { params }),
  openRemoteFile: (peer: string, path: string, readOnly = false) =>
    call<string>("open_remote_file", { peer, path, readOnly }),
  listQuickOpens: () => call<QuickOpen[]>("list_quick_opens"),
  removeQuickOpen: (space: string) => call<string | null>("remove_quick_open", { space }),
  joinSpace: (space: string, fromPeer: string) =>
    call<SpaceView>("join_space", { space, fromPeer }),
  deleteSpace: (space: string) => call<void>("delete_space", { space }),
  listConflicts: () => call<ConflictView[]>("list_conflicts"),
  resolveConflict: (
    space: string,
    mount: string,
    copyPath: string,
    keep: "current" | "copy",
  ) =>
    call<ResolveReport>("resolve_conflict", { space, mount, copyPath, keep }),
  resolveGitConflicts: (
    space: string,
    mount: string,
    gitDir: string,
    includeBranches: boolean,
  ) =>
    call<GitResolveReport>("resolve_git_conflicts", {
      space,
      mount,
      gitDir,
      includeBranches,
    }),
  listDeleteHolds: () => call<DeleteHold[]>("list_delete_holds"),
  decideDeleteHold: (
    space: string,
    decision: "apply" | "restore",
    mount?: string,
    peer?: string,
  ) =>
    call<number>("decide_delete_hold", { space, decision, mount, peer }),
  getActivity: () => call<ActivityItem[]>("get_activity"),
  pauseSync: () => call<RunnerState>("pause_sync"),
  resumeSync: () => call<RunnerState>("resume_sync"),
  checkForUpdates: () => call<UpdateInfo>("check_for_updates"),
  pendingUpdate: () => call<UpdateAvailable | null>("pending_update"),
  installUpdate: () => call<UpdateInfo>("install_update"),
  restartApp: () => call<void>("restart_app"),
  getSettings: () => call<Settings>("get_settings"),
  setSettings: (patch: Partial<Settings>) =>
    call<Settings>("set_settings", { patch }),
  openLogsFolder: () => call<void>("open_logs_folder"),
  fullDiskAccess: () => call<boolean | null>("full_disk_access"),
  openFullDiskAccess: () => call<void>("open_full_disk_access"),
  cliStatus: () => call<CliStatus>("cli_status"),
  installCli: (shell?: CliShell) =>
    call<CliInstallResult>("install_cli", { shell: shell ?? null }),
};

export function runnerLabel(state: RunnerState): string {
  switch (state.kind) {
    case "notInitialized":
      return "Not initialized";
    case "starting":
      return "Starting";
    case "running":
      return "Running";
    case "paused":
      return "Paused";
    case "stopped":
      return "Stopped";
    case "error":
      return "Error";
    case "externalService":
      return "Background service";
  }
}

export function formatTime(tsMs: number): string {
  const d = new Date(tsMs);
  return d.toLocaleTimeString(undefined, {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

export async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.position = "fixed";
    ta.style.left = "-9999px";
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
}
