use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

/// File name → content hash.
pub type Hashes = BTreeMap<String, String>;

pub fn content_hash(content: &str) -> String {
    let digest = Sha256::digest(content.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    Upload,
    Download,
    DeleteRemote,
    DeleteLocal,
    /// Both sides changed. The local copy is uploaded and the remote one is kept as a backup.
    UploadKeepingRemoteBackup,
    /// Both sides changed. The remote copy is downloaded and the local one is kept as a backup.
    DownloadKeepingLocalBackup,
}

/// Which side wins when a file changed on both sides since the last sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictWinner {
    Local,
    Remote,
}

/// Decides what to do with every file, comparing both sides against the hashes recorded at the
/// last sync (`base`). Files that are identical on both sides need nothing.
pub fn plan_sync(
    local: &Hashes,
    remote: &Hashes,
    base: &Hashes,
    conflict_winner: ConflictWinner,
) -> BTreeMap<String, SyncAction> {
    let names: BTreeSet<&String> = local.keys().chain(remote.keys()).collect();
    let mut actions = BTreeMap::new();
    for name in names {
        let local_hash = local.get(name);
        let remote_hash = remote.get(name);
        let base_hash = base.get(name);
        if local_hash == remote_hash {
            continue;
        }

        let action = if local_hash == base_hash {
            if remote_hash.is_some() {
                SyncAction::Download
            } else {
                SyncAction::DeleteLocal
            }
        } else if remote_hash == base_hash {
            if local_hash.is_some() {
                SyncAction::Upload
            } else {
                SyncAction::DeleteRemote
            }
        } else {
            match (local_hash, remote_hash) {
                // A deletion against an edit keeps the edit, so no content is lost.
                (None, Some(_)) => SyncAction::Download,
                (Some(_), None) => SyncAction::Upload,
                _ => match conflict_winner {
                    ConflictWinner::Local => SyncAction::UploadKeepingRemoteBackup,
                    ConflictWinner::Remote => SyncAction::DownloadKeepingLocalBackup,
                },
            }
        };
        actions.insert(name.clone(), action);
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn hashes(entries: &[(&str, &str)]) -> Hashes {
        entries
            .iter()
            .map(|(name, hash)| (name.to_string(), hash.to_string()))
            .collect()
    }

    #[test]
    fn test_one_sided_changes() {
        let base = hashes(&[
            ("settings.json", "a"),
            ("keymap.json", "k"),
            ("tasks.json", "t"),
        ]);
        let local = hashes(&[
            ("settings.json", "b"),
            ("keymap.json", "k"),
            ("new.json", "n"),
            ("tasks.json", "t"),
        ]);
        let remote = hashes(&[("settings.json", "a"), ("keymap.json", "k2")]);

        assert_eq!(
            plan_sync(&local, &remote, &base, ConflictWinner::Local),
            BTreeMap::from([
                ("keymap.json".to_string(), SyncAction::Download),
                ("new.json".to_string(), SyncAction::Upload),
                ("settings.json".to_string(), SyncAction::Upload),
                ("tasks.json".to_string(), SyncAction::DeleteLocal),
            ])
        );
    }

    #[test]
    fn test_local_deletion_is_propagated() {
        let base = hashes(&[("snippets--rust.json", "s")]);
        let remote = hashes(&[("snippets--rust.json", "s")]);
        assert_eq!(
            plan_sync(&Hashes::new(), &remote, &base, ConflictWinner::Local),
            BTreeMap::from([("snippets--rust.json".to_string(), SyncAction::DeleteRemote)])
        );
    }

    #[test]
    fn test_conflicts_follow_the_winner() {
        let base = hashes(&[("settings.json", "a")]);
        let local = hashes(&[("settings.json", "b")]);
        let remote = hashes(&[("settings.json", "c")]);
        assert_eq!(
            plan_sync(&local, &remote, &base, ConflictWinner::Local),
            BTreeMap::from([(
                "settings.json".to_string(),
                SyncAction::UploadKeepingRemoteBackup
            )])
        );
        assert_eq!(
            plan_sync(&local, &remote, &base, ConflictWinner::Remote),
            BTreeMap::from([(
                "settings.json".to_string(),
                SyncAction::DownloadKeepingLocalBackup
            )])
        );
    }

    #[test]
    fn test_first_sync_on_a_new_device() {
        let local = hashes(&[("settings.json", "fresh"), ("only-local.json", "l")]);
        let remote = hashes(&[("settings.json", "synced"), ("keymap.json", "k")]);
        assert_eq!(
            plan_sync(&local, &remote, &Hashes::new(), ConflictWinner::Remote),
            BTreeMap::from([
                ("keymap.json".to_string(), SyncAction::Download),
                ("only-local.json".to_string(), SyncAction::Upload),
                (
                    "settings.json".to_string(),
                    SyncAction::DownloadKeepingLocalBackup
                ),
            ])
        );
    }

    #[test]
    fn test_deletion_against_edit_keeps_the_edit() {
        let base = hashes(&[("a.json", "1"), ("b.json", "1")]);
        let local = hashes(&[("a.json", "2")]);
        let remote = hashes(&[("b.json", "2")]);
        assert_eq!(
            plan_sync(&local, &remote, &base, ConflictWinner::Local),
            BTreeMap::from([
                ("a.json".to_string(), SyncAction::Upload),
                ("b.json".to_string(), SyncAction::Download),
            ])
        );
    }
}
