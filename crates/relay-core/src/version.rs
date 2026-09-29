use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::entry::EntryContent;
use crate::ids::DeviceId;

/// Per-entry causal history: one counter per device that has written it.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionVector(BTreeMap<DeviceId, u64>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorOrdering {
    Equal,
    /// `self` has seen everything `other` has, and more.
    Dominates,
    /// `other` has seen everything `self` has, and more.
    DominatedBy,
    Concurrent,
}

impl VersionVector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, device: &DeviceId) -> u64 {
        self.0.get(device).copied().unwrap_or(0)
    }

    pub fn set(&mut self, device: DeviceId, counter: u64) {
        if counter == 0 {
            self.0.remove(&device);
        } else {
            self.0.insert(device, counter);
        }
    }

    /// Record a new write by `device` and return its counter.
    ///
    /// The counter is `max(previous + 1, now_unix_secs)`. Wall-clock time is
    /// never used to *order* versions; it only makes counters keep moving
    /// forward if this device's database is restored from an older backup, so
    /// a restored device cannot reissue a counter it already handed out.
    pub fn bump(&mut self, device: DeviceId, now_unix_secs: u64) -> u64 {
        let counter = (self.get(&device) + 1).max(now_unix_secs);
        self.0.insert(device, counter);
        counter
    }

    /// Pointwise maximum: the smallest vector that has seen both inputs.
    pub fn merged(&self, other: &VersionVector) -> VersionVector {
        let mut out = self.clone();
        for (device, counter) in &other.0 {
            let entry = out.0.entry(*device).or_insert(0);
            *entry = (*entry).max(*counter);
        }
        out
    }

    pub fn compare(&self, other: &VersionVector) -> VectorOrdering {
        let mut self_ahead = false;
        let mut other_ahead = false;
        for device in self.0.keys().chain(other.0.keys()) {
            let (a, b) = (self.get(device), other.get(device));
            self_ahead |= a > b;
            other_ahead |= b > a;
            if self_ahead && other_ahead {
                return VectorOrdering::Concurrent;
            }
        }
        match (self_ahead, other_ahead) {
            (false, false) => VectorOrdering::Equal,
            (true, false) => VectorOrdering::Dominates,
            (false, true) => VectorOrdering::DominatedBy,
            (true, true) => VectorOrdering::Concurrent,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DeviceId, u64)> {
        self.0.iter().map(|(d, c)| (d, *c))
    }
}

impl FromIterator<(DeviceId, u64)> for VersionVector {
    fn from_iter<T: IntoIterator<Item = (DeviceId, u64)>>(iter: T) -> Self {
        let mut vv = VersionVector::new();
        for (device, counter) in iter {
            vv.set(device, counter);
        }
        vv
    }
}

impl fmt::Debug for VersionVector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        for (i, (device, counter)) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}:{}", device.short(), counter)?;
        }
        f.write_str("}")
    }
}

/// What reconciling two versions of the same entry requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VersionRelation {
    /// Same history, same content. Nothing to do.
    Same,
    LocalNewer,
    RemoteNewer,
    /// Independent histories that happen to hold identical content (both
    /// machines ran the same `git pull`). Merge the vectors; not a conflict.
    ConcurrentIdentical,
    /// Real concurrent modification. Both versions must be preserved.
    Conflict,
    /// Equal vectors with different content. This can only mean a counter was
    /// reused (e.g. a database restored from backup). Treat as a conflict and
    /// report it loudly; never pick one silently.
    Diverged,
}

impl VersionRelation {
    pub fn requires_preserving_both(self) -> bool {
        matches!(self, Self::Conflict | Self::Diverged)
    }
}

pub fn compare_versions(
    local_vector: &VersionVector,
    local_content: &EntryContent,
    remote_vector: &VersionVector,
    remote_content: &EntryContent,
) -> VersionRelation {
    let same_content = local_content.same_content(remote_content);
    match (local_vector.compare(remote_vector), same_content) {
        (VectorOrdering::Equal, true) => VersionRelation::Same,
        (VectorOrdering::Equal, false) => VersionRelation::Diverged,
        (VectorOrdering::Dominates, _) => VersionRelation::LocalNewer,
        (VectorOrdering::DominatedBy, _) => VersionRelation::RemoteNewer,
        (VectorOrdering::Concurrent, true) => VersionRelation::ConcurrentIdentical,
        (VectorOrdering::Concurrent, false) => VersionRelation::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::ObjectId;
    use proptest::prelude::*;

    fn dev(n: u8) -> DeviceId {
        DeviceId::from_bytes([n; 32])
    }

    fn vv(pairs: &[(u8, u64)]) -> VersionVector {
        pairs.iter().map(|(d, c)| (dev(*d), *c)).collect()
    }

    fn file(data: &[u8]) -> EntryContent {
        EntryContent::File {
            object: ObjectId::of(data),
            size: data.len() as u64,
            executable: false,
        }
    }

    #[test]
    fn design_doc_example() {
        let (d, m) = (1, 2);
        assert_eq!(
            vv(&[(d, 2), (m, 1)]).compare(&vv(&[(d, 2)])),
            VectorOrdering::Dominates
        );
        assert_eq!(
            vv(&[(d, 3), (m, 1)]).compare(&vv(&[(d, 2), (m, 2)])),
            VectorOrdering::Concurrent
        );
        assert_eq!(vv(&[]).compare(&vv(&[])), VectorOrdering::Equal);
    }

    #[test]
    fn bump_never_goes_backwards_even_with_an_old_clock() {
        let mut v = vv(&[(1, 1_000)]);
        assert_eq!(v.bump(dev(1), 5), 1_001);
        assert_eq!(v.bump(dev(1), 2_000), 2_000);
    }

    #[test]
    fn identical_concurrent_content_is_not_a_conflict() {
        let a = vv(&[(1, 5)]);
        let b = vv(&[(2, 7)]);
        let c = file(b"same");
        assert_eq!(
            compare_versions(&a, &c, &b, &c),
            VersionRelation::ConcurrentIdentical
        );
        assert_eq!(
            compare_versions(&a, &c, &b, &file(b"different")),
            VersionRelation::Conflict
        );
    }

    #[test]
    fn equal_vectors_with_different_content_are_flagged() {
        let v = vv(&[(1, 3)]);
        let rel = compare_versions(&v, &file(b"a"), &v, &file(b"b"));
        assert_eq!(rel, VersionRelation::Diverged);
        assert!(rel.requires_preserving_both());
    }

    fn arb_vector() -> impl Strategy<Value = VersionVector> {
        proptest::collection::btree_map(0u8..4, 1u64..6, 0..4)
            .prop_map(|m| m.into_iter().map(|(d, c)| (dev(d), c)).collect())
    }

    proptest! {
        #[test]
        fn compare_is_antisymmetric(a in arb_vector(), b in arb_vector()) {
            let expected = match a.compare(&b) {
                VectorOrdering::Equal => VectorOrdering::Equal,
                VectorOrdering::Dominates => VectorOrdering::DominatedBy,
                VectorOrdering::DominatedBy => VectorOrdering::Dominates,
                VectorOrdering::Concurrent => VectorOrdering::Concurrent,
            };
            prop_assert_eq!(b.compare(&a), expected);
        }

        #[test]
        fn merge_dominates_or_equals_both(a in arb_vector(), b in arb_vector()) {
            let m = a.merged(&b);
            prop_assert!(matches!(m.compare(&a), VectorOrdering::Dominates | VectorOrdering::Equal));
            prop_assert!(matches!(m.compare(&b), VectorOrdering::Dominates | VectorOrdering::Equal));
        }

        #[test]
        fn bump_strictly_dominates(a in arb_vector(), d in 0u8..4, now in 0u64..10) {
            let mut b = a.clone();
            b.bump(dev(d), now);
            prop_assert_eq!(b.compare(&a), VectorOrdering::Dominates);
        }
    }
}
