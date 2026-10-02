use anyhow::Result;
use rusqlite::Connection;

use crate::microsoft_auth::AccountRecord;
use crate::token_storage::{
    CredentialState, EncryptedAccountsRepository, PlaintextAccountRecord, SecretStore,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedAccountProfile {
    pub microsoft_id: String,
    pub xbox_gamertag: Option<String>,
    pub minecraft_uuid: Option<String>,
    pub profile_data: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedAccountTokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
}

/// A saved account as the interface lists it: no token, only what the row
/// says about itself and whether its sign-in still works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    pub microsoft_id: String,
    pub xbox_gamertag: Option<String>,
    pub minecraft_uuid: Option<String>,
    /// Display metadata only, never a token (`account_profile_data_json`).
    pub profile_data: Option<String>,
    pub is_active: bool,
    /// The user's choice to play this account offline (D115), independent of
    /// `credentials`: a working sign-in can be set to play offline.
    pub plays_offline: bool,
    pub credentials: CredentialState,
}

pub struct AccountManager<'connection, S> {
    encrypted_accounts: EncryptedAccountsRepository<'connection, S>,
}

impl<'connection, S: SecretStore> AccountManager<'connection, S> {
    pub fn new(connection: &'connection Connection, secret_store: S) -> Self {
        Self {
            encrypted_accounts: EncryptedAccountsRepository::new(connection, secret_store),
        }
    }

    /// `replace_unreadable` is the user's explicit "Sign in again" (F1): see
    /// `EncryptedAccountsRepository::save_login`.
    pub fn save_account_login(
        &self,
        profile: ManagedAccountProfile,
        tokens: ManagedAccountTokens,
        make_active: bool,
        replace_unreadable: bool,
    ) -> Result<()> {
        self.encrypted_accounts.save_login(
            &PlaintextAccountRecord {
                microsoft_id: profile.microsoft_id,
                xbox_gamertag: profile.xbox_gamertag,
                minecraft_uuid: profile.minecraft_uuid,
                access_token: Some(tokens.access_token),
                refresh_token: tokens.refresh_token,
                profile_data: profile.profile_data,
                is_active: make_active,
            },
            replace_unreadable,
        )
    }

    /// Fails before the browser opens when the sign-in could not be saved at
    /// the end.
    pub fn check_login_can_be_saved(&self, replace_unreadable: bool) -> Result<()> {
        self.encrypted_accounts
            .check_login_can_be_saved(replace_unreadable)
    }

    pub fn switch_active_account(&self, microsoft_id: &str) -> Result<()> {
        self.encrypted_accounts.set_active_account(microsoft_id)
    }

    /// Every saved account, including those whose tokens no longer decrypt:
    /// a summary needs no token, so it reads the rows as stored and does not
    /// go through `list_accounts`, which can only return what it decrypted.
    pub fn list_account_summaries(&self) -> Result<Vec<AccountSummary>> {
        Ok(self
            .encrypted_accounts
            .list_accounts_with_credentials()?
            .into_iter()
            .map(account_summary)
            .collect())
    }

    pub fn active_account_summary(&self) -> Result<Option<AccountSummary>> {
        Ok(self
            .encrypted_accounts
            .load_active_account_with_credentials()?
            .map(account_summary))
    }

    pub fn active_author_name(&self) -> Result<Option<String>> {
        Ok(self
            .encrypted_accounts
            .load_active_account()?
            .and_then(|account| account.xbox_gamertag))
    }
}

fn account_summary((account, credentials): (AccountRecord, CredentialState)) -> AccountSummary {
    AccountSummary {
        microsoft_id: account.microsoft_id,
        xbox_gamertag: account.xbox_gamertag,
        minecraft_uuid: account.minecraft_uuid,
        profile_data: account.profile_data,
        is_active: account.is_active,
        plays_offline: account.plays_offline,
        credentials,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use crate::database::initialize_database;
    use crate::token_storage::{CredentialState, SecretStore};

    use super::{AccountManager, AccountSummary, ManagedAccountProfile, ManagedAccountTokens};

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-account-manager-test-{timestamp}"))
    }

    #[derive(Clone, Default)]
    struct MemorySecretStore {
        values: Arc<Mutex<HashMap<String, String>>>,
    }

    impl SecretStore for MemorySecretStore {
        fn get_secret(&self, key: &str) -> anyhow::Result<Option<String>> {
            Ok(self
                .values
                .lock()
                .expect("secret store mutex poisoned")
                .get(key)
                .cloned())
        }

        fn set_secret(&self, key: &str, secret: &str) -> anyhow::Result<()> {
            self.values
                .lock()
                .expect("secret store mutex poisoned")
                .insert(key.to_string(), secret.to_string());
            Ok(())
        }
    }

    fn with_manager<T>(
        test_fn: impl FnOnce(&AccountManager<'_, MemorySecretStore>, &PathBuf) -> T,
    ) -> T {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");

        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let manager = AccountManager::new(&connection, MemorySecretStore::default());

        let result = test_fn(&manager, &root_dir);

        drop(manager);
        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");

        result
    }

    #[test]
    fn saves_multiple_accounts_and_lists_summaries() {
        with_manager(|manager, _root_dir| {
            manager
                .save_account_login(
                    ManagedAccountProfile {
                        microsoft_id: "account-a".into(),
                        xbox_gamertag: Some("PlayerA".into()),
                        minecraft_uuid: Some("uuid-a".into()),
                        profile_data: Some("{\"name\":\"PlayerA\"}".into()),
                    },
                    ManagedAccountTokens {
                        access_token: "access-a".into(),
                        refresh_token: Some("refresh-a".into()),
                    },
                    true,
                    false,
                )
                .expect("first account should save");
            manager
                .save_account_login(
                    ManagedAccountProfile {
                        microsoft_id: "account-b".into(),
                        xbox_gamertag: Some("PlayerB".into()),
                        minecraft_uuid: Some("uuid-b".into()),
                        profile_data: Some("{\"name\":\"PlayerB\"}".into()),
                    },
                    ManagedAccountTokens {
                        access_token: "access-b".into(),
                        refresh_token: None,
                    },
                    false,
                    false,
                )
                .expect("second account should save");

            let summaries = manager
                .list_account_summaries()
                .expect("summaries should load");

            assert_eq!(summaries.len(), 2);
            assert!(summaries.contains(&AccountSummary {
                microsoft_id: "account-a".into(),
                xbox_gamertag: Some("PlayerA".into()),
                minecraft_uuid: Some("uuid-a".into()),
                profile_data: Some("{\"name\":\"PlayerA\"}".into()),
                is_active: true,
                plays_offline: false,
                credentials: CredentialState::Usable,
            }));
            assert!(summaries.contains(&AccountSummary {
                microsoft_id: "account-b".into(),
                xbox_gamertag: Some("PlayerB".into()),
                minecraft_uuid: Some("uuid-b".into()),
                profile_data: Some("{\"name\":\"PlayerB\"}".into()),
                is_active: false,
                plays_offline: false,
                credentials: CredentialState::SignedOut,
            }));
        });
    }

    #[test]
    fn switches_active_account_and_updates_author_name() {
        with_manager(|manager, _root_dir| {
            manager
                .save_account_login(
                    ManagedAccountProfile {
                        microsoft_id: "account-a".into(),
                        xbox_gamertag: Some("PlayerA".into()),
                        minecraft_uuid: Some("uuid-a".into()),
                        profile_data: None,
                    },
                    ManagedAccountTokens {
                        access_token: "access-a".into(),
                        refresh_token: Some("refresh-a".into()),
                    },
                    true,
                    false,
                )
                .expect("first account should save");
            manager
                .save_account_login(
                    ManagedAccountProfile {
                        microsoft_id: "account-b".into(),
                        xbox_gamertag: Some("PlayerB".into()),
                        minecraft_uuid: Some("uuid-b".into()),
                        profile_data: None,
                    },
                    ManagedAccountTokens {
                        access_token: "access-b".into(),
                        refresh_token: Some("refresh-b".into()),
                    },
                    false,
                    false,
                )
                .expect("second account should save");

            manager
                .switch_active_account("account-b")
                .expect("active account should switch");

            let active_summary = manager
                .active_account_summary()
                .expect("active summary should load")
                .expect("active summary should exist");

            assert_eq!(active_summary.microsoft_id, "account-b");
            assert_eq!(active_summary.xbox_gamertag.as_deref(), Some("PlayerB"));
            assert_eq!(
                manager.active_author_name().expect("author should load"),
                Some("PlayerB".into())
            );
        });
    }

    #[test]
    fn active_account_summary_is_none_when_no_accounts_exist() {
        with_manager(|manager, _root_dir| {
            assert!(manager
                .active_account_summary()
                .expect("summary lookup should succeed")
                .is_none());
            assert!(manager
                .active_author_name()
                .expect("author lookup should succeed")
                .is_none());
        });
    }

    /// A row whose token blob does not decrypt is still a saved account: the
    /// list must show it, not lose it (D117).
    #[test]
    fn list_account_summaries_keeps_a_row_whose_blob_does_not_decrypt() {
        with_manager(|manager, root_dir| {
            manager
                .save_account_login(
                    ManagedAccountProfile {
                        microsoft_id: "account-valid".into(),
                        xbox_gamertag: Some("ValidPlayer".into()),
                        minecraft_uuid: Some("valid-uuid".into()),
                        profile_data: None,
                    },
                    ManagedAccountTokens {
                        access_token: "valid-access".into(),
                        refresh_token: Some("valid-refresh".into()),
                    },
                    true,
                    false,
                )
                .expect("valid account should save");

            // Version byte, then a zero nonce and a zero ciphertext: the
            // shape of a blob, which no key opens.
            let mut corrupted_payload = vec![1_u8];
            corrupted_payload.extend_from_slice(&[0; 12 + 16]);
            Connection::open(root_dir.join("launcher_data.db"))
                .expect("database should open")
                .execute(
                    "INSERT INTO accounts (microsoft_id, refresh_token_enc, last_login, is_active) \
                     VALUES (?1, ?2, CURRENT_TIMESTAMP, FALSE)",
                    rusqlite::params!["account-corrupted", corrupted_payload],
                )
                .expect("corrupted account should store");

            let summaries = manager
                .list_account_summaries()
                .expect("summaries should load");
            let listed: Vec<(&str, &CredentialState)> = summaries
                .iter()
                .map(|summary| (summary.microsoft_id.as_str(), &summary.credentials))
                .collect();

            assert_eq!(summaries.len(), 2, "listed: {listed:?}");
            assert!(
                listed.contains(&("account-corrupted", &CredentialState::Unreadable)),
                "listed: {listed:?}"
            );
            assert!(
                listed.contains(&("account-valid", &CredentialState::Usable)),
                "listed: {listed:?}"
            );
        });
    }

    /// With the token key gone from the keyring no blob decrypts, which is
    /// this machine's normal state (wiki trap 15): every saved account, active
    /// or not, must still be listed so it can be chosen.
    #[test]
    fn list_account_summaries_lists_every_account_when_the_key_is_gone() {
        with_manager(|manager, root_dir| {
            for (microsoft_id, make_active) in [("account-a", true), ("account-b", false)] {
                manager
                    .save_account_login(
                        ManagedAccountProfile {
                            microsoft_id: microsoft_id.into(),
                            xbox_gamertag: Some(format!("{microsoft_id}-tag")),
                            minecraft_uuid: Some(format!("{microsoft_id}-uuid")),
                            profile_data: None,
                        },
                        ManagedAccountTokens {
                            access_token: format!("access-{microsoft_id}"),
                            refresh_token: Some(format!("refresh-{microsoft_id}")),
                        },
                        make_active,
                        false,
                    )
                    .expect("account should save");
            }

            let connection =
                Connection::open(root_dir.join("launcher_data.db")).expect("database should open");
            let keyless = AccountManager::new(&connection, MemorySecretStore::default());
            let summaries = keyless
                .list_account_summaries()
                .expect("summaries should load");
            let listed: Vec<(&str, bool, &CredentialState)> = summaries
                .iter()
                .map(|summary| {
                    (
                        summary.microsoft_id.as_str(),
                        summary.is_active,
                        &summary.credentials,
                    )
                })
                .collect();

            assert_eq!(summaries.len(), 2, "listed: {listed:?}");
            assert!(
                listed.contains(&("account-a", true, &CredentialState::Unreadable)),
                "listed: {listed:?}"
            );
            assert!(
                listed.contains(&("account-b", false, &CredentialState::Unreadable)),
                "listed: {listed:?}"
            );
        });
    }
}
