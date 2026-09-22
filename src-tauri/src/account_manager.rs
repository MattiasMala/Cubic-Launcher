use anyhow::Result;
use rusqlite::Connection;

use crate::token_storage::{EncryptedAccountsRepository, PlaintextAccountRecord, SecretStore};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    pub microsoft_id: String,
    pub xbox_gamertag: Option<String>,
    pub minecraft_uuid: Option<String>,
    pub is_active: bool,
    pub has_refresh_token: bool,
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

    pub fn list_account_summaries(&self) -> Result<Vec<AccountSummary>> {
        self.encrypted_accounts
            .list_accounts()?
            .into_iter()
            .map(account_summary_from_plaintext)
            .collect()
    }

    pub fn active_account_summary(&self) -> Result<Option<AccountSummary>> {
        self.encrypted_accounts
            .load_active_account()?
            .map(account_summary_from_plaintext)
            .transpose()
    }

    pub fn active_author_name(&self) -> Result<Option<String>> {
        Ok(self
            .encrypted_accounts
            .load_active_account()?
            .and_then(|account| account.xbox_gamertag))
    }
}

fn account_summary_from_plaintext(account: PlaintextAccountRecord) -> Result<AccountSummary> {
    Ok(AccountSummary {
        microsoft_id: account.microsoft_id,
        xbox_gamertag: account.xbox_gamertag,
        minecraft_uuid: account.minecraft_uuid,
        is_active: account.is_active,
        has_refresh_token: account
            .refresh_token
            .as_deref()
            .map(str::trim)
            .is_some_and(|token| !token.is_empty()),
    })
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
    use crate::token_storage::SecretStore;

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
                is_active: true,
                has_refresh_token: true,
            }));
            assert!(summaries.contains(&AccountSummary {
                microsoft_id: "account-b".into(),
                xbox_gamertag: Some("PlayerB".into()),
                minecraft_uuid: Some("uuid-b".into()),
                is_active: false,
                has_refresh_token: false,
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
}
