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
  MountView,
  OfferView,
  Overview,
  PeerView,
  ResolveReport,
  RunnerState,
  Settings,
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
  pairStart: (share: string[]) => call<PairStartResult>("pair_start", { share }),
  pairStatus: () => call<PairStatus>("pair_status"),
  pairJoin: (code: string, addr?: string) =>
    call<PairJoinResult>("pair_join", { code, addr: addr || null }),
  pairCancel: () => call<void>("pair_cancel"),
  listSpaces: () => call<SpaceView[]>("list_spaces"),
  createSpace: (name: string) => call<SpaceView>("create_space", { name }),
  addMount: (space: string, mount: string, path: string) =>
    call<MountView>("add_mount", { space, mount, path }),
  share: (space: string, peer: string) => call<void>("share", { space, peer }),
  unshare: (space: string, peer: string) => call<void>("unshare", { space, peer }),
  listOffers: () => call<OfferView[]>("list_offers"),
  joinSpace: (space: string, fromPeer: string) =>
    call<SpaceView>("join_space", { space, fromPeer }),
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
