//! Snapshot of a mount's working tree, minus Relay's own files.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use relay_core::ObjectId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    File(Vec<u8>),
    Dir,
}

/// Logical path (forward slashes) to content. Relay's marker and temp files
/// (`.relay-*`) are left out.
pub type Tree = BTreeMap<String, Node>;

pub fn snapshot(root: &Path) -> Tree {
    let mut out = Tree::new();
    walk(root, root, &mut out);
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Tree) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(".relay-") {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .expect("entry is under the root")
            .to_string_lossy()
            .replace('\\', "/");
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            out.insert(rel, Node::Dir);
            walk(root, &path, out);
        } else if meta.is_file()
            && let Ok(bytes) = fs::read(&path)
        {
            out.insert(rel, Node::File(bytes));
        }
    }
}

pub fn hash(bytes: &[u8]) -> ObjectId {
    ObjectId::from(blake3::hash(bytes))
}

pub fn files(tree: &Tree) -> impl Iterator<Item = (&String, &Vec<u8>)> {
    tree.iter().filter_map(|(path, node)| match node {
        Node::File(bytes) => Some((path, bytes)),
        Node::Dir => None,
    })
}

pub fn dirs(tree: &Tree) -> impl Iterator<Item = &String> {
    tree.iter().filter_map(|(path, node)| match node {
        Node::Dir => Some(path),
        Node::File(_) => None,
    })
}

/// A short, stable description of a tree for failure messages.
pub fn describe(tree: &Tree) -> String {
    let mut lines = Vec::new();
    for (path, node) in tree {
        match node {
            Node::Dir => lines.push(format!("{path}/")),
            Node::File(bytes) => lines.push(format!(
                "{path} ({} bytes, {})",
                bytes.len(),
                &hash(bytes).to_string()[..8]
            )),
        }
    }
    lines.join("\n")
}
