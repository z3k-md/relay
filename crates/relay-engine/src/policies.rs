//! Device groups and replication policies (D27).

use relay_core::{DeviceId, PolicyId, SpaceId, validate_name};
use relay_db::{DeviceGroupRecord, PolicyRecord, SnapshotPolicy};
use relay_policy::{selector_matches, validate_selector};

use crate::Engine;
use crate::error::EngineError;

use serde::Serialize;

/// A local policy with group targets expanded for display / offers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PolicyInfo {
    pub id: PolicyId,
    pub space: String,
    pub name: String,
    pub selectors: Vec<String>,
    pub targets: Vec<DeviceId>,
    pub peer_names: Vec<String>,
    pub group_names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GroupInfo {
    pub name: String,
    pub members: Vec<String>,
}

impl Engine {
    pub fn group_create(&mut self, name: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        validate_name(name)?;
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| repo.create_device_group(name, now))
            .map_err(EngineError::from_db)
    }

    pub fn group_add(&mut self, name: &str, peer_or_self: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let device = self.resolve_device_name(peer_or_self)?;
        self.db
            .transaction(|repo| {
                repo.add_group_member(name, device)
                    .map_err(|err| match err {
                        relay_db::DbError::NotFound => EngineError::UnknownGroup(name.to_owned()),
                        other => EngineError::from_db(other),
                    })?;
                for space in repo.spaces_referencing_group(name)? {
                    repo.bump_policy_epoch(space)?;
                }
                Ok(())
            })
            .map_err(|err| match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            })
    }

    pub fn group_remove_member(
        &mut self,
        name: &str,
        peer_or_self: &str,
    ) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let device = self.resolve_device_name(peer_or_self)?;
        self.db
            .transaction(|repo| {
                repo.remove_group_member(name, device)
                    .map_err(|err| match err {
                        relay_db::DbError::NotFound => EngineError::UnknownGroup(name.to_owned()),
                        other => EngineError::from_db(other),
                    })?;
                for space in repo.spaces_referencing_group(name)? {
                    repo.bump_policy_epoch(space)?;
                }
                Ok(())
            })
            .map_err(|err| match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            })
    }

    pub fn group_delete(&mut self, name: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        self.db
            .transaction(|repo| {
                let spaces = repo.spaces_referencing_group(name)?;
                repo.delete_device_group(name).map_err(|err| match err {
                    relay_db::DbError::NotFound => EngineError::UnknownGroup(name.to_owned()),
                    other => EngineError::from_db(other),
                })?;
                for space in spaces {
                    repo.bump_policy_epoch(space)?;
                }
                Ok(())
            })
            .map_err(|err| match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            })
    }

    pub fn groups(&self) -> Result<Vec<GroupInfo>, EngineError> {
        let groups = self.db.repo().list_device_groups()?;
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            out.push(self.group_info(group)?);
        }
        Ok(out)
    }

    pub fn policy_add(
        &mut self,
        space: &str,
        name: &str,
        selectors: &[String],
        peer_names: &[String],
        group_names: &[String],
    ) -> Result<PolicyInfo, EngineError> {
        self.ensure_writable()?;
        validate_name(name)?;
        if selectors.is_empty() || (peer_names.is_empty() && group_names.is_empty()) {
            return Err(EngineError::EmptyPolicy);
        }
        for selector in selectors {
            validate_selector(selector)?;
        }
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        let mut peers = Vec::new();
        for peer in peer_names {
            peers.push(self.resolve_device_name(peer)?);
        }
        for group in group_names {
            let known = self
                .db
                .repo()
                .list_device_groups()?
                .iter()
                .any(|g| g.name == *group);
            if !known {
                return Err(EngineError::UnknownGroup(group.clone()));
            }
        }
        let id = PolicyId::new();
        let now = self.clock.now_ms();
        self.db
            .transaction(|repo| {
                repo.create_policy(id, space_rec.id, name, selectors, &peers, group_names, now)?;
                repo.bump_policy_epoch(space_rec.id)?;
                Ok(())
            })
            .map_err(EngineError::from_db)?;
        self.policies(Some(space))?
            .into_iter()
            .find(|p| p.name == name)
            .ok_or_else(|| EngineError::UnknownPolicy(name.to_owned()))
    }

    pub fn policy_remove(&mut self, space: &str, name: &str) -> Result<(), EngineError> {
        self.ensure_writable()?;
        let space_rec = self
            .db
            .repo()
            .space_by_name(space)?
            .ok_or_else(|| EngineError::UnknownSpace(space.to_owned()))?;
        self.db
            .transaction(|repo| {
                repo.delete_policy(space_rec.id, name)
                    .map_err(|err| match err {
                        relay_db::DbError::NotFound => EngineError::UnknownPolicy(name.to_owned()),
                        other => EngineError::from_db(other),
                    })?;
                repo.bump_policy_epoch(space_rec.id)?;
                Ok(())
            })
            .map_err(|err| match err {
                EngineError::Db(inner) => EngineError::from_db(inner),
                other => other,
            })
    }

    pub fn policies(&self, space: Option<&str>) -> Result<Vec<PolicyInfo>, EngineError> {
        let spaces = match space {
            Some(name) => {
                let rec = self
                    .db
                    .repo()
                    .space_by_name(name)?
                    .ok_or_else(|| EngineError::UnknownSpace(name.to_owned()))?;
                vec![rec]
            }
            None => self.db.repo().list_spaces()?,
        };
        let mut out = Vec::new();
        for space_rec in spaces {
            for policy in self.db.repo().list_policies(space_rec.id)? {
                out.push(self.policy_info(&space_rec.name, &policy)?);
            }
        }
        out.sort_by(|a, b| a.space.cmp(&b.space).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    /// Whether `device` should receive paths in `space`: the policy
    /// selectors that route content to it, read once so a whole batch is
    /// tested without re-reading the policies.
    ///
    /// Share is still required separately. With no effective policies the
    /// answer is always true.
    pub(crate) fn wants_resolver(
        &self,
        space: SpaceId,
        device: DeviceId,
    ) -> Result<WantsResolver, EngineError> {
        let mut any = false;
        let mut selectors = Vec::new();
        for policy in self.db.repo().list_policies(space)? {
            any = true;
            let targets = self.db.repo().expand_policy_targets(&policy)?;
            if targets.contains(&device) {
                selectors.extend(policy.selectors);
            }
        }
        for snap in self.db.repo().all_peer_policy_snapshots_for_space(space)? {
            any = true;
            if snap.targets.contains(&device) {
                selectors.extend(snap.selectors);
            }
        }
        Ok(WantsResolver { any, selectors })
    }

    /// Local policies for a space with targets expanded (for SpaceOffer).
    pub(crate) fn local_policy_offers(
        &self,
        space: SpaceId,
    ) -> Result<(u64, Vec<relay_proto::PolicyOffer>), EngineError> {
        let epoch = self.db.repo().policy_epoch(space)?;
        let mut policies = Vec::new();
        for policy in self.db.repo().list_policies(space)? {
            let targets = self.db.repo().expand_policy_targets(&policy)?;
            policies.push(relay_proto::PolicyOffer {
                id: relay_proto::policy_id_bytes(&policy.id),
                name: policy.name,
                selectors: policy.selectors,
                targets: targets
                    .into_iter()
                    .map(|id| id.as_bytes().to_vec())
                    .collect(),
            });
        }
        Ok((epoch, policies))
    }

    pub(crate) fn store_peer_policy_snapshot(
        &mut self,
        peer: DeviceId,
        space: SpaceId,
        epoch: u64,
        policies: &[relay_proto::PolicyOffer],
    ) -> Result<(), EngineError> {
        let mut snaps = Vec::new();
        for p in policies {
            let id = relay_proto::policy_id_from_bytes(&p.id)?;
            let mut targets = Vec::new();
            for t in &p.targets {
                targets.push(relay_proto::device_id_from_bytes(t)?);
            }
            snaps.push(SnapshotPolicy {
                id,
                name: p.name.clone(),
                selectors: p.selectors.clone(),
                targets,
            });
        }
        self.db
            .transaction(|repo| repo.put_peer_policy_snapshot(peer, space, epoch, &snaps))
            .map_err(EngineError::from_db)
    }

    fn resolve_device_name(&self, name: &str) -> Result<DeviceId, EngineError> {
        if self.device.name == name {
            return Ok(self.device.id);
        }
        self.find_peer(name)?
            .map(|p| p.device.id)
            .ok_or_else(|| EngineError::UnknownPeer(name.to_owned()))
    }

    fn device_display_name(&self, id: DeviceId) -> Result<String, EngineError> {
        if id == self.device.id {
            return Ok(self.device.name.clone());
        }
        if let Some(peer) = self.db.repo().peer_by_id(id)? {
            return Ok(peer.device.name);
        }
        Ok(id.short())
    }

    fn group_info(&self, group: DeviceGroupRecord) -> Result<GroupInfo, EngineError> {
        let mut members = Vec::with_capacity(group.members.len());
        for id in group.members {
            members.push(self.device_display_name(id)?);
        }
        members.sort();
        Ok(GroupInfo {
            name: group.name,
            members,
        })
    }

    fn policy_info(&self, space: &str, policy: &PolicyRecord) -> Result<PolicyInfo, EngineError> {
        let targets = self.db.repo().expand_policy_targets(policy)?;
        let mut peer_names = Vec::new();
        for id in &policy.peer_targets {
            peer_names.push(self.device_display_name(*id)?);
        }
        peer_names.sort();
        Ok(PolicyInfo {
            id: policy.id,
            space: space.to_owned(),
            name: policy.name.clone(),
            selectors: policy.selectors.clone(),
            targets,
            peer_names,
            group_names: policy.group_targets.clone(),
        })
    }
}

/// See [`Engine::wants_resolver`].
pub(crate) struct WantsResolver {
    /// Whether any policy applies to the space at all; with none, every
    /// device receives everything.
    any: bool,
    selectors: Vec<String>,
}

impl WantsResolver {
    pub(crate) fn wants(&self, mount_name: &str, relative_path: &str) -> bool {
        if !self.any {
            return true;
        }
        let path = policy_path(mount_name, relative_path);
        self.selectors
            .iter()
            .any(|selector| selector_matches(selector, &path).unwrap_or(false))
    }
}

pub(crate) fn policy_path(mount_name: &str, relative_path: &str) -> String {
    if relative_path.is_empty() {
        mount_name.to_owned()
    } else {
        format!("{mount_name}/{relative_path}")
    }
}

/// Collect effective policy presence for unit tests / debugging.
#[cfg(test)]
pub(crate) fn effective_has_policies(local: &[PolicyRecord], snaps: &[SnapshotPolicy]) -> bool {
    !local.is_empty() || !snaps.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use relay_policy::selector_matches;

    #[test]
    fn selector_match_and_case_sensitivity() {
        assert!(selector_matches("code/personal/**", "code/personal/a.txt").unwrap());
        assert!(!selector_matches("code/personal/**", "code/work/a.txt").unwrap());
        assert!(!selector_matches("code/Personal/**", "code/personal/a.txt").unwrap());
        assert!(selector_matches("code/*/a.txt", "code/personal/a.txt").unwrap());
        assert!(!selector_matches("code/*/a.txt", "code/personal/nested/a.txt").unwrap());
    }

    #[test]
    fn union_of_two_selectors() {
        let path = "code/work/b.txt";
        let hit = ["code/personal/**", "code/work/**"]
            .iter()
            .any(|s| selector_matches(s, path).unwrap());
        assert!(hit);
        let miss = ["code/personal/**"]
            .iter()
            .any(|s| selector_matches(s, path).unwrap());
        assert!(!miss);
    }

    #[test]
    fn empty_effective_means_want_all() {
        assert!(!effective_has_policies(&[], &[]));
    }
}
