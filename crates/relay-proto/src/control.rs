//! Wire form of remote calls (`relay_core::remote`, D37).
//!
//! A call rides its own bidirectional stream: the caller writes one
//! [`ObjectRequest`](crate::ObjectRequest) whose `control` field is set and
//! finishes its side; the answering device writes one [`ControlResponse`].
//! Calls go only to peers whose [`Hello`](crate::Hello) advertises
//! [`FEATURE_CONTROL`], so an older peer never sees one.
//!
//! A config change and its result travel as their JSON form, the same schema
//! local IPC uses, so the change has one definition (`ConfigChange`) instead
//! of a second one in protobuf. A change a peer cannot decode is `invalid`.

use relay_core::remote::{
    DirEntry, DirEntryKind, DirListing, Located, MountRef, MountedPath, PathPreview, RemoteCall,
    RemoteError, RemoteErrorCode, RemoteMount, RemoteReply, RemoteResult, RemoteRoot, RemoteSpace,
};

use crate::{Empty, ProtoError, invalid};

/// `Hello.features` bit: this device answers remote calls and sends
/// [`PeerGrants`].
pub const FEATURE_CONTROL: u64 = 1;

/// Whether the sender lets the receiver manage it. Sent after `Hello` and
/// whenever the grant changes, only to peers with [`FEATURE_CONTROL`].
#[derive(Clone, Copy, PartialEq, prost::Message)]
pub struct PeerGrants {
    #[prost(bool, tag = "1")]
    pub may_manage_you: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ControlRequest {
    #[prost(oneof = "control_request::Call", tags = "1, 2, 3, 4, 5, 6, 7, 8, 9")]
    pub call: Option<control_request::Call>,
}

pub mod control_request {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Call {
        #[prost(message, tag = "1")]
        Roots(super::Empty),
        #[prost(message, tag = "2")]
        ListDir(super::WireListDir),
        #[prost(string, tag = "3")]
        Stat(String),
        #[prost(message, tag = "4")]
        Spaces(super::Empty),
        #[prost(string, tag = "5")]
        Preview(String),
        #[prost(message, tag = "6")]
        CreateDir(super::WireCreateDir),
        /// `ConfigChange` as JSON.
        #[prost(bytes = "vec", tag = "7")]
        Apply(Vec<u8>),
        #[prost(string, tag = "8")]
        Locate(String),
        #[prost(message, tag = "9")]
        ScanFirst(super::WireMountedPath),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireListDir {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(uint32, tag = "2")]
    pub cursor: u32,
    #[prost(uint32, tag = "3")]
    pub limit: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireCreateDir {
    #[prost(string, tag = "1")]
    pub parent: String,
    #[prost(string, tag = "2")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ControlResponse {
    #[prost(
        oneof = "control_response::Reply",
        tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 15"
    )]
    pub reply: Option<control_response::Reply>,
}

pub mod control_response {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Reply {
        #[prost(message, tag = "1")]
        Roots(super::WireRoots),
        #[prost(message, tag = "2")]
        Listing(super::WireListing),
        #[prost(message, tag = "3")]
        Stat(super::WireDirEntry),
        #[prost(message, tag = "4")]
        Spaces(super::WireSpaces),
        #[prost(message, tag = "5")]
        Preview(super::WirePreview),
        #[prost(message, tag = "6")]
        Created(super::WireDirEntry),
        /// `ConfigApplied` as JSON.
        #[prost(bytes = "vec", tag = "7")]
        Applied(Vec<u8>),
        #[prost(message, tag = "8")]
        Located(super::WireLocated),
        #[prost(message, tag = "9")]
        Done(super::Empty),
        #[prost(message, tag = "15")]
        Error(super::WireRemoteError),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireRoots {
    #[prost(message, repeated, tag = "1")]
    pub roots: Vec<WireRoot>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireRoot {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireListing {
    #[prost(string, tag = "1")]
    pub path: String,
    #[prost(string, optional, tag = "2")]
    pub parent: Option<String>,
    #[prost(message, repeated, tag = "3")]
    pub entries: Vec<WireDirEntry>,
    #[prost(uint32, optional, tag = "4")]
    pub next_cursor: Option<u32>,
    #[prost(uint32, tag = "5")]
    pub total: u32,
    #[prost(message, optional, tag = "6")]
    pub inside_mount: Option<WireMountRef>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireDirEntry {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub path: String,
    /// 0 file, 1 directory, 2 symlink, anything else other.
    #[prost(int32, tag = "3")]
    pub kind: i32,
    #[prost(uint64, optional, tag = "4")]
    pub size: Option<u64>,
    #[prost(int64, optional, tag = "5")]
    pub modified_ms: Option<i64>,
    #[prost(bool, tag = "6")]
    pub hidden: bool,
    #[prost(bool, tag = "7")]
    pub cloud_only: bool,
    #[prost(message, optional, tag = "8")]
    pub mount: Option<WireMountRef>,
    #[prost(bool, tag = "9")]
    pub contains_mount: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireMountRef {
    #[prost(string, tag = "1")]
    pub space: String,
    #[prost(string, tag = "2")]
    pub mount: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireSpaces {
    #[prost(message, repeated, tag = "1")]
    pub spaces: Vec<WireSpace>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireSpace {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, repeated, tag = "2")]
    pub mounts: Vec<WireMount>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireMount {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, optional, tag = "2")]
    pub path: Option<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WirePreview {
    #[prost(string, optional, tag = "1")]
    pub path: Option<String>,
    #[prost(bool, tag = "2")]
    pub exists: bool,
    #[prost(bool, tag = "3")]
    pub is_dir: bool,
    #[prost(uint64, tag = "4")]
    pub files: u64,
    #[prost(uint64, tag = "5")]
    pub bytes: u64,
    #[prost(bool, tag = "6")]
    pub truncated: bool,
    #[prost(message, optional, tag = "7")]
    pub overlaps: Option<WireMountRef>,
    #[prost(bool, tag = "8")]
    pub cloud_only: bool,
    #[prost(bool, tag = "9")]
    pub writable: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireMountedPath {
    #[prost(string, tag = "1")]
    pub space: String,
    #[prost(string, tag = "2")]
    pub mount: String,
    #[prost(string, tag = "3")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireLocated {
    #[prost(string, tag = "1")]
    pub folder: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(uint64, tag = "3")]
    pub size: u64,
    #[prost(message, optional, tag = "4")]
    pub mount: Option<WireMountedPath>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WireRemoteError {
    #[prost(string, tag = "1")]
    pub code: String,
    #[prost(string, tag = "2")]
    pub message: String,
}

pub fn call_to_wire(call: &RemoteCall) -> ControlRequest {
    use control_request::Call;
    let call = match call {
        RemoteCall::Roots => Call::Roots(Empty {}),
        RemoteCall::ListDir {
            path,
            cursor,
            limit,
        } => Call::ListDir(WireListDir {
            path: path.clone(),
            cursor: *cursor,
            limit: *limit,
        }),
        RemoteCall::Stat { path } => Call::Stat(path.clone()),
        RemoteCall::Spaces => Call::Spaces(Empty {}),
        RemoteCall::Preview { path } => Call::Preview(path.clone()),
        RemoteCall::CreateDir { parent, name } => Call::CreateDir(WireCreateDir {
            parent: parent.clone(),
            name: name.clone(),
        }),
        RemoteCall::Apply { change } => {
            Call::Apply(serde_json::to_vec(change).expect("a ConfigChange always serializes"))
        }
        RemoteCall::Locate { path } => Call::Locate(path.clone()),
        RemoteCall::ScanFirst { space, mount, path } => Call::ScanFirst(WireMountedPath {
            space: space.clone(),
            mount: mount.clone(),
            path: path.clone(),
        }),
    };
    ControlRequest { call: Some(call) }
}

pub fn call_from_wire(request: ControlRequest) -> Result<RemoteCall, ProtoError> {
    use control_request::Call;
    Ok(
        match request.call.ok_or_else(|| invalid("call", "missing"))? {
            Call::Roots(_) => RemoteCall::Roots,
            Call::ListDir(list) => RemoteCall::ListDir {
                path: list.path,
                cursor: list.cursor,
                limit: list.limit,
            },
            Call::Stat(path) => RemoteCall::Stat { path },
            Call::Spaces(_) => RemoteCall::Spaces,
            Call::Preview(path) => RemoteCall::Preview { path },
            Call::CreateDir(dir) => RemoteCall::CreateDir {
                parent: dir.parent,
                name: dir.name,
            },
            Call::Apply(json) => RemoteCall::Apply {
                change: serde_json::from_slice(&json)
                    .map_err(|e| invalid("change", e.to_string()))?,
            },
            Call::Locate(path) => RemoteCall::Locate { path },
            Call::ScanFirst(at) => RemoteCall::ScanFirst {
                space: at.space,
                mount: at.mount,
                path: at.path,
            },
        },
    )
}

pub fn result_to_wire(result: &RemoteResult) -> ControlResponse {
    use control_response::Reply;
    let reply = match result {
        Ok(RemoteReply::Roots { roots }) => Reply::Roots(WireRoots {
            roots: roots
                .iter()
                .map(|root| WireRoot {
                    name: root.name.clone(),
                    path: root.path.clone(),
                })
                .collect(),
        }),
        Ok(RemoteReply::Listing { listing }) => Reply::Listing(WireListing {
            path: listing.path.clone(),
            parent: listing.parent.clone(),
            entries: listing.entries.iter().map(entry_to_wire).collect(),
            next_cursor: listing.next_cursor,
            total: listing.total,
            inside_mount: listing.inside_mount.as_ref().map(mount_ref_to_wire),
        }),
        Ok(RemoteReply::Stat { entry }) => Reply::Stat(entry_to_wire(entry)),
        Ok(RemoteReply::Spaces { spaces }) => Reply::Spaces(WireSpaces {
            spaces: spaces
                .iter()
                .map(|space| WireSpace {
                    name: space.name.clone(),
                    mounts: space
                        .mounts
                        .iter()
                        .map(|mount| WireMount {
                            name: mount.name.clone(),
                            path: mount.path.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }),
        Ok(RemoteReply::Preview { preview }) => Reply::Preview(WirePreview {
            path: preview.path.clone(),
            exists: preview.exists,
            is_dir: preview.is_dir,
            files: preview.files,
            bytes: preview.bytes,
            truncated: preview.truncated,
            overlaps: preview.overlaps.as_ref().map(mount_ref_to_wire),
            cloud_only: preview.cloud_only,
            writable: preview.writable,
        }),
        Ok(RemoteReply::Created { entry }) => Reply::Created(entry_to_wire(entry)),
        Ok(RemoteReply::Applied { applied }) => {
            Reply::Applied(serde_json::to_vec(applied).expect("a ConfigApplied always serializes"))
        }
        Ok(RemoteReply::Located { located }) => Reply::Located(WireLocated {
            folder: located.folder.clone(),
            name: located.name.clone(),
            size: located.size,
            mount: located.mount.as_ref().map(|m| WireMountedPath {
                space: m.space.clone(),
                mount: m.mount.clone(),
                path: m.path.clone(),
            }),
        }),
        Ok(RemoteReply::Done) => Reply::Done(Empty {}),
        Err(err) => Reply::Error(error_to_wire(err)),
    };
    ControlResponse { reply: Some(reply) }
}

pub fn result_from_wire(response: ControlResponse) -> Result<RemoteResult, ProtoError> {
    use control_response::Reply;
    Ok(
        match response.reply.ok_or_else(|| invalid("reply", "missing"))? {
            Reply::Roots(roots) => Ok(RemoteReply::Roots {
                roots: roots
                    .roots
                    .into_iter()
                    .map(|root| RemoteRoot {
                        name: root.name,
                        path: root.path,
                    })
                    .collect(),
            }),
            Reply::Listing(listing) => Ok(RemoteReply::Listing {
                listing: DirListing {
                    path: listing.path,
                    parent: listing.parent,
                    entries: listing.entries.into_iter().map(entry_from_wire).collect(),
                    next_cursor: listing.next_cursor,
                    total: listing.total,
                    inside_mount: listing.inside_mount.map(mount_ref_from_wire),
                },
            }),
            Reply::Stat(entry) => Ok(RemoteReply::Stat {
                entry: entry_from_wire(entry),
            }),
            Reply::Spaces(spaces) => Ok(RemoteReply::Spaces {
                spaces: spaces
                    .spaces
                    .into_iter()
                    .map(|space| RemoteSpace {
                        name: space.name,
                        mounts: space
                            .mounts
                            .into_iter()
                            .map(|mount| RemoteMount {
                                name: mount.name,
                                path: mount.path,
                            })
                            .collect(),
                    })
                    .collect(),
            }),
            Reply::Preview(p) => Ok(RemoteReply::Preview {
                preview: PathPreview {
                    path: p.path,
                    exists: p.exists,
                    is_dir: p.is_dir,
                    files: p.files,
                    bytes: p.bytes,
                    truncated: p.truncated,
                    overlaps: p.overlaps.map(mount_ref_from_wire),
                    cloud_only: p.cloud_only,
                    writable: p.writable,
                },
            }),
            Reply::Created(entry) => Ok(RemoteReply::Created {
                entry: entry_from_wire(entry),
            }),
            Reply::Applied(json) => Ok(RemoteReply::Applied {
                applied: serde_json::from_slice(&json)
                    .map_err(|e| invalid("applied", e.to_string()))?,
            }),
            Reply::Located(found) => Ok(RemoteReply::Located {
                located: Located {
                    folder: found.folder,
                    name: found.name,
                    size: found.size,
                    mount: found.mount.map(|m| MountedPath {
                        space: m.space,
                        mount: m.mount,
                        path: m.path,
                    }),
                },
            }),
            Reply::Done(_) => Ok(RemoteReply::Done),
            Reply::Error(err) => Err(error_from_wire(err)),
        },
    )
}

pub fn error_to_wire(err: &RemoteError) -> WireRemoteError {
    WireRemoteError {
        code: err.code.as_str().to_owned(),
        message: err.message.clone(),
    }
}

/// An unknown code from a newer peer reads as `failed`.
pub fn error_from_wire(err: WireRemoteError) -> RemoteError {
    RemoteError::new(RemoteErrorCode::parse(&err.code), err.message)
}

fn entry_to_wire(entry: &DirEntry) -> WireDirEntry {
    WireDirEntry {
        name: entry.name.clone(),
        path: entry.path.clone(),
        kind: match entry.kind {
            DirEntryKind::File => 0,
            DirEntryKind::Directory => 1,
            DirEntryKind::Symlink => 2,
            DirEntryKind::Other => 3,
        },
        size: entry.size,
        modified_ms: entry.modified_ms,
        hidden: entry.hidden,
        cloud_only: entry.cloud_only,
        mount: entry.mount.as_ref().map(mount_ref_to_wire),
        contains_mount: entry.contains_mount,
    }
}

fn entry_from_wire(entry: WireDirEntry) -> DirEntry {
    DirEntry {
        name: entry.name,
        path: entry.path,
        kind: match entry.kind {
            0 => DirEntryKind::File,
            1 => DirEntryKind::Directory,
            2 => DirEntryKind::Symlink,
            _ => DirEntryKind::Other,
        },
        size: entry.size,
        modified_ms: entry.modified_ms,
        hidden: entry.hidden,
        cloud_only: entry.cloud_only,
        mount: entry.mount.map(mount_ref_from_wire),
        contains_mount: entry.contains_mount,
    }
}

fn mount_ref_to_wire(mount: &MountRef) -> WireMountRef {
    WireMountRef {
        space: mount.space.clone(),
        mount: mount.mount.clone(),
    }
}

fn mount_ref_from_wire(mount: WireMountRef) -> MountRef {
    MountRef {
        space: mount.space,
        mount: mount.mount,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectRequest, decode_message, encode_frame};

    fn round_trip(result: RemoteResult) {
        let bytes = encode_frame(&result_to_wire(&result)).unwrap();
        let decoded: ControlResponse = decode_message(&bytes[4..]).unwrap();
        assert_eq!(result_from_wire(decoded).unwrap(), result);
    }

    #[test]
    fn calls_and_results_round_trip() {
        for call in [
            RemoteCall::Roots,
            RemoteCall::ListDir {
                path: "C:\\Users\\zach".into(),
                cursor: 2_000,
                limit: 500,
            },
            RemoteCall::Stat {
                path: "/tmp".into(),
            },
            RemoteCall::Spaces,
            RemoteCall::Preview {
                path: "D:\\Games".into(),
            },
            RemoteCall::CreateDir {
                parent: "/Users/zach".into(),
                name: "xyz-foo".into(),
            },
            RemoteCall::Apply {
                change: relay_core::ConfigChange::Share {
                    space: "S".into(),
                    peer: "ab".repeat(32),
                },
            },
        ] {
            assert_eq!(call_from_wire(call_to_wire(&call)).unwrap(), call);
        }
        round_trip(Ok(RemoteReply::Listing {
            listing: DirListing {
                path: "/Users/zach".into(),
                parent: Some("/Users".into()),
                entries: vec![DirEntry {
                    name: "Code".into(),
                    path: "/Users/zach/Code".into(),
                    kind: DirEntryKind::Directory,
                    size: None,
                    modified_ms: Some(1_700_000_000_000),
                    hidden: false,
                    cloud_only: false,
                    mount: Some(MountRef {
                        space: "Projects".into(),
                        mount: "code".into(),
                    }),
                    contains_mount: false,
                }],
                next_cursor: None,
                total: 1,
                inside_mount: None,
            },
        }));
        round_trip(Ok(RemoteReply::Located {
            located: Located {
                folder: "C:\\Users\\zach\\Documents".into(),
                name: "report.docx".into(),
                size: 42,
                mount: Some(MountedPath {
                    space: "Docs".into(),
                    mount: "docs".into(),
                    path: "work/report.docx".into(),
                }),
            },
        }));
        round_trip(Ok(RemoteReply::Done));
        round_trip(Ok(RemoteReply::Applied {
            applied: relay_core::ConfigApplied::Done,
        }));
        round_trip(Err(RemoteError::new(RemoteErrorCode::Forbidden, "no")));
    }

    /// An old object request decodes with no control call, and a control
    /// request decodes with an empty object id, so the two never mix up.
    #[test]
    fn control_rides_the_object_request() {
        let old = encode_frame(&ObjectRequest {
            object_id: vec![7; 32],
            ..Default::default()
        })
        .unwrap();
        let decoded: ObjectRequest = decode_message(&old[4..]).unwrap();
        assert!(decoded.control.is_none());

        let call = encode_frame(&ObjectRequest {
            control: Some(call_to_wire(&RemoteCall::Roots)),
            ..Default::default()
        })
        .unwrap();
        let decoded: ObjectRequest = decode_message(&call[4..]).unwrap();
        assert!(decoded.object_id.is_empty());
        assert!(decoded.control.is_some());
    }
}
