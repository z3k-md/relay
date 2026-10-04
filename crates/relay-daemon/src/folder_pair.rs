//! Set up a folder pair across devices from this one (remote explorer
//! Stage 3).
//!
//! Each step is a remote call on the device it touches: this device answers
//! its own steps through [`remote::answer`], a peer answers through the
//! network. That keeps one code path whether this device is the source, the
//! destination, or neither. Steps that change something are recorded, and a
//! failure undoes them in reverse.

use relay_core::remote::{PathPreview, RemoteCall, RemoteError, RemoteReply, RemoteResult};
use relay_core::{ConfigApplied, ConfigChange, DeviceId, validate_name};
use relay_engine::Engine;
use relay_ipc::{
    ActivityItem, FolderEnd, FolderPairParams, FolderPairPlan, FolderPairResult, RpcErrorBody,
};

use crate::host::{Host, now_ms};
use crate::remote;

/// How long the destination waits for the source's offer. Below the
/// network's 10 s per-call limit so the join answers in time.
const JOIN_WAIT_MS: u64 = 8_000;

/// Which device a step runs on.
#[derive(Clone)]
enum Target {
    Here { id: DeviceId },
    Peer { id: DeviceId, name: String },
}

impl Target {
    fn id(&self) -> DeviceId {
        match self {
            Self::Here { id } | Self::Peer { id, .. } => *id,
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Here { .. } => "this device",
            Self::Peer { name, .. } => name,
        }
    }

    fn call(&self, host: &Host, call: RemoteCall) -> RemoteResult {
        match self {
            Self::Here { .. } => remote::answer(host, &host.home, call, "this device"),
            Self::Peer { id, .. } => host.call_peer(*id, call),
        }
    }

    fn apply(&self, host: &Host, change: ConfigChange) -> Result<ConfigApplied, RemoteError> {
        match self.call(host, RemoteCall::Apply { change })? {
            RemoteReply::Applied { applied } => Ok(applied),
            other => Err(unexpected(&other)),
        }
    }

    fn preview(&self, host: &Host, path: &str) -> Result<PathPreview, RemoteError> {
        match self.call(
            host,
            RemoteCall::Preview {
                path: path.to_owned(),
            },
        )? {
            RemoteReply::Preview { preview } => Ok(preview),
            other => Err(unexpected(&other)),
        }
    }

    fn space_names(&self, host: &Host) -> Result<Vec<String>, RemoteError> {
        match self.call(host, RemoteCall::Spaces)? {
            RemoteReply::Spaces { spaces } => Ok(spaces.into_iter().map(|s| s.name).collect()),
            other => Err(unexpected(&other)),
        }
    }
}

struct Ends {
    source: Target,
    dest: Target,
}

pub(crate) fn preview(
    host: &Host,
    params: &FolderPairParams,
) -> Result<FolderPairPlan, RpcErrorBody> {
    let ends = resolve(host, params)?;
    plan(host, &ends, params).map_err(rpc)
}

pub(crate) fn run(
    host: &Host,
    params: &FolderPairParams,
) -> Result<FolderPairResult, RpcErrorBody> {
    let ends = resolve(host, params)?;
    let plan = plan(host, &ends, params).map_err(rpc)?;
    if let Some(problem) = plan.problems.first() {
        return Err(RpcErrorBody::new("conflict", problem.clone()));
    }
    let mut undo = Vec::new();
    match execute(host, &ends, params, &plan, &mut undo) {
        Ok(result) => {
            host.push_activity(ActivityItem {
                at_ms: now_ms(),
                kind: "folder_pair".into(),
                summary: format!(
                    "set up sync for {} on {} and {} on {}",
                    result.source_path,
                    ends.source.label(),
                    result.dest_path,
                    ends.dest.label()
                ),
                detail: Some(result.space.clone()),
            });
            Ok(result)
        }
        Err(err) => {
            for (target, change) in undo.into_iter().rev() {
                if let Err(undo_err) = target.apply(host, change) {
                    tracing::warn!(device = target.label(), error = %undo_err, "could not undo a folder pair step");
                }
            }
            Err(rpc(err))
        }
    }
}

fn resolve(host: &Host, params: &FolderPairParams) -> Result<Ends, RpcErrorBody> {
    let here = Engine::open_read_only(&host.home)
        .map_err(|err| RpcErrorBody::new("unavailable", err.to_string()))?
        .device()
        .id;
    let target = |end: &FolderEnd| -> Result<Target, RpcErrorBody> {
        Ok(match &end.device {
            None => Target::Here { id: here },
            Some(name) => {
                let peer = host.peer_named(name)?;
                Target::Peer {
                    id: peer.id,
                    name: peer.name,
                }
            }
        })
    };
    let ends = Ends {
        source: target(&params.source)?,
        dest: target(&params.dest)?,
    };
    if ends.source.id() == ends.dest.id() {
        return Err(RpcErrorBody::new(
            "invalid",
            "pick a folder on two different devices",
        ));
    }
    Ok(ends)
}

fn plan(
    host: &Host,
    ends: &Ends,
    params: &FolderPairParams,
) -> Result<FolderPairPlan, RemoteError> {
    let mut problems = Vec::new();
    let mut warnings = Vec::new();
    let (source_name, dest_name) = (ends.source.label(), ends.dest.label());

    let source = ends.source.preview(host, &params.source.path)?;
    if !source.exists || !source.is_dir {
        problems.push(format!("That folder is not on {source_name}."));
    }
    if let Some(mount) = &source.overlaps {
        problems.push(format!(
            "On {source_name}, that folder is inside or around {}/{}, which already syncs.",
            mount.space, mount.mount
        ));
    }
    if source.cloud_only {
        warnings.push(format!(
            "On {source_name}, that folder holds cloud placeholders; syncing downloads them all."
        ));
    }

    let dest = match &params.create_dest {
        Some(new_name) => {
            let parent = ends.dest.preview(host, &params.dest.path)?;
            if !parent.is_dir || !parent.writable {
                problems.push(format!("Cannot create {new_name} there on {dest_name}."));
            }
            if parent.overlaps.is_some() {
                problems.push(format!(
                    "On {dest_name}, that location is inside a folder that already syncs."
                ));
            }
            None
        }
        None => {
            let dest = ends.dest.preview(host, &params.dest.path)?;
            if !dest.exists || !dest.is_dir {
                problems.push(format!("The destination is not a folder on {dest_name}."));
            } else if !dest.writable {
                problems.push(format!("{dest_name} cannot write to the destination."));
            }
            if let Some(mount) = &dest.overlaps {
                problems.push(format!(
                    "On {dest_name}, the destination is inside or around {}/{}, which already syncs.",
                    mount.space, mount.mount
                ));
            }
            if dest.files > 0 {
                let at_least = if dest.truncated { "at least " } else { "" };
                warnings.push(format!(
                    "The destination already has {at_least}{} files. Files that differ become conflict copies; nothing is deleted.",
                    dest.files
                ));
            }
            Some(dest)
        }
    };

    let taken: Vec<String> = [&ends.source, &ends.dest]
        .into_iter()
        .map(|target| target.space_names(host))
        .collect::<Result<Vec<_>, _>>()?
        .concat();
    let wanted = params
        .name
        .clone()
        .unwrap_or_else(|| folder_name(source.path.as_deref().unwrap_or(&params.source.path)));
    Ok(FolderPairPlan {
        space: unique_name(&wanted, &taken),
        source,
        dest,
        problems,
        warnings,
    })
}

fn execute(
    host: &Host,
    ends: &Ends,
    params: &FolderPairParams,
    plan: &FolderPairPlan,
    undo: &mut Vec<(Target, ConfigChange)>,
) -> Result<FolderPairResult, RemoteError> {
    let (source, dest) = (&ends.source, &ends.dest);
    let space = plan.space.clone();
    // One mount per pair, named like the space.
    let mount = space.clone();
    let source_path = plan
        .source
        .path
        .clone()
        .unwrap_or_else(|| params.source.path.clone());

    let ConfigApplied::Space { space: created } = source.apply(
        host,
        ConfigChange::CreateSpace {
            space: space.clone(),
        },
    )?
    else {
        return Err(RemoteError::new(
            relay_core::remote::RemoteErrorCode::Failed,
            "creating the space returned no space",
        ));
    };
    let delete_space = ConfigChange::DeleteSpace {
        space: space.clone(),
    };
    undo.push((source.clone(), delete_space.clone()));
    source.apply(host, add_mount(&space, &mount, &source_path))?;
    undo.push((source.clone(), remove_mount(&space, &mount)));
    for excluded in &params.excludes {
        source.apply(host, folder_mode(&space, &mount, excluded, Some("exclude")))?;
    }
    source
        .apply(
            host,
            ConfigChange::Share {
                space: space.clone(),
                peer: dest.id().to_string(),
            },
        )
        .map_err(|err| needs_pairing(err, source, dest))?;
    undo.push((
        source.clone(),
        ConfigChange::Unshare {
            space: space.clone(),
            peer: dest.id().to_string(),
        },
    ));

    let dest_path = match &params.create_dest {
        Some(name) => match dest.call(
            host,
            RemoteCall::CreateDir {
                parent: params.dest.path.clone(),
                name: name.clone(),
            },
        )? {
            RemoteReply::Created { entry } => entry.path,
            other => return Err(unexpected(&other)),
        },
        None => plan
            .dest
            .as_ref()
            .and_then(|d| d.path.clone())
            .unwrap_or_else(|| params.dest.path.clone()),
    };
    dest.apply(
        host,
        ConfigChange::JoinSpace {
            space: created.id.to_string(),
            from_peer: source.id().to_string(),
            wait_ms: JOIN_WAIT_MS,
        },
    )?;
    undo.push((dest.clone(), delete_space));
    // Before attaching, so nothing downloads that should not.
    for excluded in &params.excludes {
        dest.apply(host, folder_mode(&space, &mount, excluded, Some("exclude")))?;
    }
    if params.dest_online_only {
        dest.apply(host, folder_mode(&space, &mount, "", Some("demand")))?;
    }
    dest.apply(host, add_mount(&space, &mount, &dest_path))?;
    undo.push((dest.clone(), remove_mount(&space, &mount)));

    Ok(FolderPairResult {
        space,
        source_path,
        dest_path,
    })
}

fn add_mount(space: &str, mount: &str, path: &str) -> ConfigChange {
    ConfigChange::AddMount {
        space: space.to_owned(),
        mount: mount.to_owned(),
        path: path.into(),
        includes: Vec::new(),
        excludes: Vec::new(),
    }
}

fn remove_mount(space: &str, mount: &str) -> ConfigChange {
    ConfigChange::RemoveMount {
        space: space.to_owned(),
        mount: mount.to_owned(),
    }
}

fn folder_mode(space: &str, mount: &str, path: &str, mode: Option<&str>) -> ConfigChange {
    ConfigChange::SetFolderMode {
        space: space.to_owned(),
        mount: mount.to_owned(),
        path: path.to_owned(),
        mode: mode.map(str::to_owned),
    }
}

/// The last component of a path in either platform's form. For naming only;
/// paths themselves are never built here.
fn folder_name(path: &str) -> String {
    let name = path
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .trim_end_matches(':');
    let name: String = name.chars().take(60).collect();
    if validate_name(&name).is_ok() {
        name
    } else {
        "Folder".to_owned()
    }
}

/// `wanted`, or `wanted 2`, `wanted 3`, … if a device already has that space.
fn unique_name(wanted: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == wanted) {
        return wanted.to_owned();
    }
    (2..)
        .map(|n| format!("{wanted} {n}"))
        .find(|candidate| !taken.contains(candidate))
        .expect("an unused name exists")
}

fn needs_pairing(err: RemoteError, source: &Target, dest: &Target) -> RemoteError {
    if err.code == relay_core::remote::RemoteErrorCode::NotFound {
        RemoteError::new(
            err.code,
            format!(
                "{} and {} are not paired with each other. Pair them first.",
                source.label(),
                dest.label()
            ),
        )
    } else {
        err
    }
}

fn unexpected(reply: &RemoteReply) -> RemoteError {
    RemoteError::new(
        relay_core::remote::RemoteErrorCode::Failed,
        format!("unexpected reply {reply:?}"),
    )
}

fn rpc(err: RemoteError) -> RpcErrorBody {
    RpcErrorBody::new(err.code.as_str(), err.message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_the_last_component_of_either_path_style() {
        assert_eq!(folder_name("C:\\Users\\zach\\xyz"), "xyz");
        assert_eq!(folder_name("/Users/zach/xyz-foo/"), "xyz-foo");
        assert_eq!(folder_name("D:\\"), "D");
        assert_eq!(folder_name("/"), "Folder");
    }

    #[test]
    fn taken_names_get_a_number() {
        let taken = vec!["xyz".to_owned(), "xyz 2".to_owned()];
        assert_eq!(unique_name("xyz", &taken), "xyz 3");
        assert_eq!(unique_name("other", &taken), "other");
    }
}
