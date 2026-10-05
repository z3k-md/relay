//! What the workload wrote, so convergence can be checked for data loss.
//!
//! The model does not predict the engine's outcome. It records, per path and
//! per node, the last thing that node wrote since the previous converged
//! state together with what that node's run of edits started from, and drops
//! a record once another node is seen building on it (its edit started from
//! a tree that already carried it). At convergence every surviving record
//! must be present: as the file, as a conflict copy, or, for text, as its
//! changed lines inside a merged result.

use std::collections::{BTreeMap, BTreeSet};

use relay_core::ObjectId;
use relay_core::conflict::CONFLICT_MARKER;

use crate::tree::{self, Node, Tree};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Last {
    Content {
        /// This node's run of edits, oldest first; the last is current.
        run: Vec<Vec<u8>>,
        /// What the run started from, if the path existed. Advances when
        /// another node is seen building on an older edit of the run.
        parent: Option<Vec<u8>>,
    },
    Deleted {
        /// Another node wrote the path without having seen this delete, or
        /// deleted a different version of it (so one of the two deletes
        /// raced the edit that produced the other's). The engine keeps the
        /// live side of such a race, so the path may stay.
        contested: bool,
        /// What was deleted, if the deleter saw a file there.
        deleted: Option<Vec<u8>>,
    },
}

#[derive(Clone, Debug, Default)]
pub struct PathModel {
    pub last: Vec<Option<Last>>,
}

#[derive(Debug)]
pub struct Model {
    nodes: usize,
    paths: BTreeMap<String, PathModel>,
    /// Every content the workload ever wrote, by hash.
    pub known: BTreeSet<ObjectId>,
}

fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn lines(bytes: &[u8]) -> BTreeSet<&[u8]> {
    bytes.split_inclusive(|b| *b == b'\n').collect()
}

impl Model {
    pub fn new(nodes: usize) -> Self {
        Self {
            nodes,
            paths: BTreeMap::new(),
            known: BTreeSet::new(),
        }
    }

    /// Lines of `content` that are not in `parent`.
    fn changed_lines<'a>(content: &'a [u8], parent: Option<&[u8]>) -> BTreeSet<&'a [u8]> {
        let mut changed = lines(content);
        if let Some(parent) = parent {
            for line in lines(parent) {
                changed.remove(line);
            }
        }
        changed
    }

    /// `candidate` carries everything `content` contributed: it is the same
    /// bytes, or a text merge that kept all of its changed lines.
    fn carries(content: &[u8], parent: Option<&[u8]>, candidate: &[u8]) -> bool {
        if content == candidate {
            return true;
        }
        if !is_text(content) || !is_text(candidate) {
            return false;
        }
        let changed = Self::changed_lines(content, parent);
        let have = lines(candidate);
        changed.iter().all(|line| have.contains(line))
    }

    /// Drop or trim the records at one path that `cur` builds on.
    fn supersede(entry: &mut PathModel, cur: Option<&[u8]>, skip: Option<usize>) {
        for (k, slot) in entry.last.iter_mut().enumerate() {
            if Some(k) == skip {
                continue;
            }
            match slot {
                Some(Last::Content { run, parent }) => {
                    let Some(cur) = cur else { continue };
                    let last = run.last().expect("run is never empty");
                    if Self::carries(last, parent.as_deref(), cur) {
                        // The writer built on everything `k` wrote here.
                        *slot = None;
                    } else if let Some(i) = run.iter().rposition(|c| c.as_slice() == cur) {
                        // The writer built on an older edit of the run: only
                        // the edits after it are still outstanding.
                        *parent = Some(run[i].clone());
                        run.drain(..=i);
                    }
                }
                Some(Last::Deleted { .. }) if cur.is_none() => *slot = None,
                Some(Last::Deleted { .. }) | None => {}
            }
        }
    }

    /// Node `node` wrote `new` (or deleted, when `None`) at `path`, having
    /// seen `cur` there just before.
    pub fn write(&mut self, path: &str, node: usize, cur: Option<&[u8]>, new: Option<&[u8]>) {
        if let Some(bytes) = new {
            self.known.insert(tree::hash(bytes));
        }
        let nodes = self.nodes;
        // Editing a conflict copy builds on the version it preserved, at
        // every level of nesting.
        let mut original = path;
        while let Some(idx) = original.rfind(CONFLICT_MARKER) {
            original = &original[..idx];
            if let Some(entry) = self.paths.get_mut(original) {
                Self::supersede(entry, cur, None);
            }
        }
        let entry = self
            .paths
            .entry(path.to_owned())
            .or_insert_with(|| PathModel {
                last: vec![None; nodes],
            });
        Self::supersede(entry, cur, Some(node));
        // What survives supersession on the other nodes is concurrent with
        // this write: a delete racing a write there may lose to it.
        let others_live = entry
            .last
            .iter()
            .enumerate()
            .any(|(k, l)| k != node && matches!(l, Some(Last::Content { .. })));
        let mut other_deletes_differ = false;
        if cur.is_some() {
            for (k, slot) in entry.last.iter_mut().enumerate() {
                if k != node
                    && let Some(Last::Deleted { contested, deleted }) = slot
                {
                    if new.is_some() {
                        *contested = true;
                    } else if deleted.is_some() && deleted.as_deref() != cur {
                        *contested = true;
                        other_deletes_differ = true;
                    }
                }
            }
        }
        match (new, &mut entry.last[node]) {
            (Some(bytes), Some(Last::Content { run, .. }))
                if cur == run.last().map(Vec::as_slice) =>
            {
                run.push(bytes.to_vec());
            }
            (Some(bytes), slot) => {
                *slot = Some(Last::Content {
                    run: vec![bytes.to_vec()],
                    parent: cur.map(<[u8]>::to_vec),
                });
            }
            (None, slot) => {
                *slot = Some(Last::Deleted {
                    contested: others_live || other_deletes_differ,
                    deleted: cur.map(<[u8]>::to_vec),
                })
            }
        }
    }

    /// Every record must be visible in the converged `tree`.
    pub fn check(&self, tree: &Tree) -> Result<(), String> {
        for (path, model) in &self.paths {
            let mut candidates: Vec<&Vec<u8>> = Vec::new();
            if let Some(Node::File(bytes)) = tree.get(path) {
                candidates.push(bytes);
            }
            // Conflict copies of this path, nested ones included.
            let copy_prefix = format!("{path}{CONFLICT_MARKER}");
            for (other, node) in tree {
                if let Node::File(bytes) = node
                    && other.starts_with(&copy_prefix)
                {
                    candidates.push(bytes);
                }
            }
            let mut any_live = false;
            let mut any_deleted = false;
            let mut contested = false;
            for (node, last) in model.last.iter().enumerate() {
                match last {
                    None => {}
                    Some(Last::Deleted { contested: c, .. }) => {
                        any_deleted = true;
                        contested |= c;
                    }
                    Some(Last::Content { run, parent }) => {
                        let content = run.last().expect("run is never empty");
                        any_live = true;
                        if !candidates
                            .iter()
                            .any(|c| Self::carries(content, parent.as_deref(), c))
                        {
                            let changed: Vec<String> = if is_text(content) {
                                Self::changed_lines(content, parent.as_deref())
                                    .iter()
                                    .map(|l| String::from_utf8_lossy(l).trim_end().to_owned())
                                    .collect()
                            } else {
                                vec!["<binary>".into()]
                            };
                            return Err(format!(
                                "data loss: node{node}'s last write of {path} ({} bytes, {}) is neither the file, a conflict copy, nor merged into one; its new lines {changed:?} are in none of {} candidates; converged tree:\n{}",
                                content.len(),
                                &tree::hash(content).to_string()[..8],
                                candidates.len(),
                                tree::describe(tree)
                            ));
                        }
                    }
                }
            }
            if any_deleted && !any_live && !contested && tree.contains_key(path) {
                return Err(format!(
                    "resurrection: {path} was deleted and nothing wrote it since, but it is in the converged tree:\n{}",
                    tree::describe(tree)
                ));
            }
        }
        Ok(())
    }

    /// The trees converged: every record has been accounted for.
    pub fn reset(&mut self) {
        self.paths.clear();
    }
}
