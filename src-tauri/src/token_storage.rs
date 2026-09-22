use std::cell::Cell;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use rand::rngs::OsRng;
use rand::RngCore;
use rusqlite::Connection;

use crate::microsoft_auth::{AccountRecord, AccountsRepository};

const TOKEN_ENCRYPTION_VERSION: u8 = 1;
const TOKEN_NONCE_LENGTH: usize = 12;
const TOKEN_KEY_LENGTH: usize = 32;
const TOKEN_KEY_ID: &str = "microsoft-token-key-v1";
const KEYRING_SERVICE_NAME: &str = "com.cubic.launcher";

pub trait SecretStore {
    fn get_secret(&self, key: &str) -> Result<Option<String>>;
    fn set_secret(&self, key: &str, secret: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct KeyringSecretStore {
    service_name: String,
}

impl KeyringSecretStore {
    pub fn new() -> Self {
        Self {
            service_name: KEYRING_SERVICE_NAME.to_string(),
        }
    }
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretStore for KeyringSecretStore {
    fn get_secret(&self, key: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(&self.service_name, key)?;

        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(anyhow!(error.to_string())),
        }
    }

    fn set_secret(&self, key: &str, secret: &str) -> Result<()> {
        let entry = keyring::Entry::new(&self.service_name, key)?;
        entry
            .set_password(secret)
            .map_err(|error| anyhow!(error.to_string()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaintextAccountRecord {
    pub microsoft_id: String,
    pub xbox_gamertag: Option<String>,
    pub minecraft_uuid: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub profile_data: Option<String>,
    pub is_active: bool,
}

/// What the keyring answers about the token key.
///
/// `Missing` is the keyring's own answer ("no such entry"), not a failure to
/// ask. With a real backend that answer is certain, and since every blob is
/// encrypted under the one key id, no saved blob can ever be decrypted again.
/// `Damaged` is an entry that is not a key: it decrypts nothing either.
/// `Unavailable` is the other case: the keyring could not be asked at all (no
/// Secret Service, an unlock prompt dismissed), and nothing is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKeyStatus {
    Present,
    Missing,
    Damaged,
    Unavailable(String),
}

/// Whether a saved account can still go online without the browser (F1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialState {
    /// The refresh token decrypts: launches authenticate.
    Usable,
    /// No refresh token is saved: launches are offline until a sign-in.
    SignedOut,
    /// A refresh token is saved and the keyring answered, but it cannot be
    /// decrypted: the key is gone or the blob is damaged. Only a new sign-in
    /// fixes it.
    Unreadable,
    /// The keyring could not be asked; the detail says why.
    KeyringUnavailable(String),
}

impl CredentialState {
    /// The name the frontend receives.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Usable => "usable",
            Self::SignedOut => "signed_out",
            Self::Unreadable => "unreadable",
            Self::KeyringUnavailable(_) => "keyring_unavailable",
        }
    }
}

pub struct AccountTokenCipher<S> {
    secret_store: S,
    key_id: String,
    key_creation_blocked: Cell<bool>,
}

impl<S: SecretStore> AccountTokenCipher<S> {
    pub fn new(secret_store: S) -> Self {
        Self {
            secret_store,
            key_id: TOKEN_KEY_ID.to_string(),
            key_creation_blocked: Cell::new(false),
        }
    }

    pub fn encrypt_token(&self, token: &str) -> Result<Vec<u8>> {
        self.encrypt_token_with_key_creation(token, true)
    }

    fn encrypt_token_with_key_creation(
        &self,
        token: &str,
        create_key_if_missing: bool,
    ) -> Result<Vec<u8>> {
        let cipher = self.cipher(create_key_if_missing && !self.key_creation_blocked.get())?;
        encrypt_payload(&cipher, token)
    }

    pub fn decrypt_token(&self, payload: &[u8]) -> Result<String> {
        let cipher = match self.cipher(false) {
            Ok(cipher) => cipher,
            Err(error) => {
                self.key_creation_blocked.set(true);
                return Err(error);
            }
        };
        decrypt_payload(&cipher, payload)
    }

    /// Asks the keyring once, without creating anything.
    pub fn key_status(&self) -> TokenKeyStatus {
        match self.secret_store.get_secret(&self.key_id) {
            Ok(Some(encoded_key)) => match decode_key_bytes(&encoded_key) {
                Ok(_) => TokenKeyStatus::Present,
                Err(_) => TokenKeyStatus::Damaged,
            },
            Ok(None) => TokenKeyStatus::Missing,
            Err(error) => TokenKeyStatus::Unavailable(format!("{error:#}")),
        }
    }

    /// Classifies an account's saved tokens with one keyring round trip. Never
    /// creates a key and never blocks later key creation: it only looks.
    ///
    /// The refresh token decides `Usable`, but a lone access blob counts too:
    /// the save guard looks at both columns, so an unreadable access blob must
    /// show as `Unreadable` (with its way out), not as a plain `SignedOut`.
    pub fn credential_state(
        &self,
        refresh_token_enc: Option<&[u8]>,
        access_token_enc: Option<&[u8]>,
    ) -> CredentialState {
        let refresh = refresh_token_enc.filter(|payload| !payload.is_empty());
        let Some(payload) = refresh.or(access_token_enc.filter(|payload| !payload.is_empty()))
        else {
            return CredentialState::SignedOut;
        };
        let encoded_key = match self.secret_store.get_secret(&self.key_id) {
            Ok(Some(encoded_key)) => encoded_key,
            Ok(None) => return CredentialState::Unreadable,
            Err(error) => return CredentialState::KeyringUnavailable(format!("{error:#}")),
        };
        let readable = decode_key_bytes(&encoded_key)
            .and_then(|key_bytes| decrypt_payload(&cipher_from_key(&key_bytes), payload))
            .is_ok();
        match (readable, refresh.is_some()) {
            (false, _) => CredentialState::Unreadable,
            (true, true) => CredentialState::Usable,
            (true, false) => CredentialState::SignedOut,
        }
    }

    /// Stores a brand-new key, replacing whatever the keyring held under the
    /// key id. Only `save_login` calls this, and only after `key_status` said
    /// that no saved blob can be decrypted by the current key anyway.
    fn replace_key(&self) -> Result<Aes256Gcm> {
        Ok(cipher_from_key(&self.store_new_key()?))
    }

    fn cipher(&self, create_key_if_missing: bool) -> Result<Aes256Gcm> {
        let key_bytes = self.load_key(create_key_if_missing)?;
        Ok(cipher_from_key(&key_bytes))
    }

    fn load_key(&self, create_if_missing: bool) -> Result<[u8; TOKEN_KEY_LENGTH]> {
        if let Some(encoded_key) = self.secret_store.get_secret(&self.key_id)? {
            return decode_key_bytes(&encoded_key);
        }

        if !create_if_missing {
            bail!(
                "stored account credential key is unavailable; saved accounts cannot be decrypted and the account must be added again"
            );
        }

        self.store_new_key()
    }

    fn store_new_key(&self) -> Result<[u8; TOKEN_KEY_LENGTH]> {
        let mut key_bytes = [0_u8; TOKEN_KEY_LENGTH];
        OsRng.fill_bytes(&mut key_bytes);
        self.secret_store
            .set_secret(&self.key_id, &STANDARD.encode(key_bytes))?;
        Ok(key_bytes)
    }
}

pub struct EncryptedAccountsRepository<'connection, S> {
    accounts_repository: AccountsRepository<'connection>,
    token_cipher: AccountTokenCipher<S>,
}

impl<'connection, S: SecretStore> EncryptedAccountsRepository<'connection, S> {
    pub fn new(connection: &'connection Connection, secret_store: S) -> Self {
        Self {
            accounts_repository: AccountsRepository::new(connection),
            token_cipher: AccountTokenCipher::new(secret_store),
        }
    }

    pub fn upsert_account(&self, account: &PlaintextAccountRecord) -> Result<()> {
        let create_key_if_missing = !self.accounts_repository.has_encrypted_account_tokens()?;
        self.accounts_repository.upsert_account(&AccountRecord {
            microsoft_id: account.microsoft_id.clone(),
            xbox_gamertag: account.xbox_gamertag.clone(),
            minecraft_uuid: account.minecraft_uuid.clone(),
            access_token_enc: account
                .access_token
                .as_deref()
                .map(|token| {
                    self.token_cipher
                        .encrypt_token_with_key_creation(token, create_key_if_missing)
                })
                .transpose()?,
            refresh_token_enc: account
                .refresh_token
                .as_deref()
                .map(|token| {
                    self.token_cipher
                        .encrypt_token_with_key_creation(token, create_key_if_missing)
                })
                .transpose()?,
            profile_data: account.profile_data.clone(),
            is_active: account.is_active,
        })
    }

    /// Answers, before the browser opens, whether a sign-in could be saved at
    /// the end. Without it the refusal in `upsert_account` arrived only after
    /// the whole OAuth round trip (F1).
    pub fn check_login_can_be_saved(&self, replace_unreadable: bool) -> Result<()> {
        self.login_save_plan(replace_unreadable).map(|_| ())
    }

    /// Saves a fresh sign-in. `replace_unreadable` is the user's explicit
    /// "Sign in again": when the keyring answers that the key is missing or
    /// damaged, no saved blob can be decrypted by anyone, so every token blob
    /// is set to NULL (the account rows, names and UUIDs stay) and a new key
    /// is born. Without that gesture the refusal of `upsert_account` stands.
    /// A keyring that cannot be asked discards nothing: it is an error.
    pub fn save_login(&self, account: &PlaintextAccountRecord, replace_unreadable: bool) -> Result<()> {
        match self.login_save_plan(replace_unreadable)? {
            LoginSavePlan::Upsert => self.upsert_account(account),
            LoginSavePlan::ReplaceUnreadable => {
                // Key first: if the database write fails afterwards, the old
                // blobs stay unreadable next to a present key, and the next
                // sign-in takes the ordinary path.
                let cipher = self.token_cipher.replace_key()?;
                let access_token_enc = account
                    .access_token
                    .as_deref()
                    .map(|token| encrypt_payload(&cipher, token))
                    .transpose()?;
                let refresh_token_enc = account
                    .refresh_token
                    .as_deref()
                    .map(|token| encrypt_payload(&cipher, token))
                    .transpose()?;
                self.accounts_repository.discard_encrypted_tokens()?;
                self.accounts_repository.upsert_account(&AccountRecord {
                    microsoft_id: account.microsoft_id.clone(),
                    xbox_gamertag: account.xbox_gamertag.clone(),
                    minecraft_uuid: account.minecraft_uuid.clone(),
                    access_token_enc,
                    refresh_token_enc,
                    profile_data: account.profile_data.clone(),
                    is_active: account.is_active,
                })
            }
        }
    }

    fn login_save_plan(&self, replace_unreadable: bool) -> Result<LoginSavePlan> {
        match self.token_cipher.key_status() {
            TokenKeyStatus::Present => Ok(LoginSavePlan::Upsert),
            TokenKeyStatus::Unavailable(detail) => bail!(
                "The system keyring can't be reached, so a sign-in could not be saved ({detail}); make sure a keyring service is running and unlocked, then try again"
            ),
            TokenKeyStatus::Missing | TokenKeyStatus::Damaged => {
                // With no blob saved, a new key orphans nothing.
                if replace_unreadable || !self.accounts_repository.has_encrypted_account_tokens()? {
                    Ok(LoginSavePlan::ReplaceUnreadable)
                } else {
                    bail!(
                        "The saved sign-ins can't be read any more, and a new one can't be stored next to them; use \"Sign in again\" on the account in Manage Accounts, which replaces them"
                    )
                }
            }
        }
    }

    pub fn load_active_account(&self) -> Result<Option<PlaintextAccountRecord>> {
        self.accounts_repository
            .load_active_account()?
            .map(|account| self.decrypt_account(account))
            .transpose()
    }

    pub fn list_accounts(&self) -> Result<Vec<PlaintextAccountRecord>> {
        Ok(self
            .accounts_repository
            .list_accounts()?
            .into_iter()
            .filter_map(|account| self.decrypt_account(account).ok())
            .collect())
    }

    pub fn set_active_account(&self, microsoft_id: &str) -> Result<()> {
        self.accounts_repository.set_active_account(microsoft_id)
    }

    fn decrypt_account(&self, account: AccountRecord) -> Result<PlaintextAccountRecord> {
        Ok(PlaintextAccountRecord {
            microsoft_id: account.microsoft_id,
            xbox_gamertag: account.xbox_gamertag,
            minecraft_uuid: account.minecraft_uuid,
            access_token: account
                .access_token_enc
                .as_deref()
                .map(|payload| self.token_cipher.decrypt_token(payload))
                .transpose()?,
            refresh_token: account
                .refresh_token_enc
                .as_deref()
                .map(|payload| self.token_cipher.decrypt_token(payload))
                .transpose()?,
            profile_data: account.profile_data,
            is_active: account.is_active,
        })
    }
}

enum LoginSavePlan {
    Upsert,
    ReplaceUnreadable,
}

fn cipher_from_key(key_bytes: &[u8; TOKEN_KEY_LENGTH]) -> Aes256Gcm {
    Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key_bytes))
}

fn encrypt_payload(cipher: &Aes256Gcm, token: &str) -> Result<Vec<u8>> {
    let mut nonce_bytes = [0_u8; TOKEN_NONCE_LENGTH];
    OsRng.fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, token.as_bytes())
        .map_err(|_| anyhow!("failed to encrypt account token"))?;

    let mut payload = Vec::with_capacity(1 + TOKEN_NONCE_LENGTH + ciphertext.len());
    payload.push(TOKEN_ENCRYPTION_VERSION);
    payload.extend_from_slice(&nonce_bytes);
    payload.extend_from_slice(&ciphertext);

    Ok(payload)
}

fn decrypt_payload(cipher: &Aes256Gcm, payload: &[u8]) -> Result<String> {
    if payload.len() <= 1 + TOKEN_NONCE_LENGTH {
        bail!("encrypted token payload is too short");
    }

    if payload[0] != TOKEN_ENCRYPTION_VERSION {
        bail!("unsupported encrypted token payload version {}", payload[0]);
    }

    let nonce = Nonce::from_slice(&payload[1..1 + TOKEN_NONCE_LENGTH]);
    let ciphertext = &payload[1 + TOKEN_NONCE_LENGTH..];
    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| anyhow!("failed to decrypt account token"))?;

    String::from_utf8(plaintext).context("decrypted account token is not valid UTF-8")
}

fn decode_key_bytes(encoded_key: &str) -> Result<[u8; TOKEN_KEY_LENGTH]> {
    let decoded = STANDARD
        .decode(encoded_key)
        .context("failed to decode stored token encryption key")?;

    if decoded.len() != TOKEN_KEY_LENGTH {
        bail!(
            "stored token encryption key must be {} bytes, got {}",
            TOKEN_KEY_LENGTH,
            decoded.len()
        );
    }

    let mut key_bytes = [0_u8; TOKEN_KEY_LENGTH];
    key_bytes.copy_from_slice(&decoded);
    Ok(key_bytes)
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

    use super::{
        AccountTokenCipher, CredentialState, EncryptedAccountsRepository, PlaintextAccountRecord,
        SecretStore, TOKEN_ENCRYPTION_VERSION, TOKEN_NONCE_LENGTH,
    };

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-token-storage-test-{timestamp}"))
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

    #[test]
    fn encrypts_and_decrypts_token_roundtrip() {
        let cipher = AccountTokenCipher::new(MemorySecretStore::default());

        let encrypted = cipher
            .encrypt_token("access-token-value")
            .expect("token should encrypt");
        let decrypted = cipher
            .decrypt_token(&encrypted)
            .expect("token should decrypt");

        assert_ne!(encrypted, b"access-token-value");
        assert_eq!(decrypted, "access-token-value");
    }

    #[test]
    fn persisted_secret_store_key_allows_future_decryption() {
        let secret_store = MemorySecretStore::default();
        let first_cipher = AccountTokenCipher::new(secret_store.clone());
        let second_cipher = AccountTokenCipher::new(secret_store);

        let encrypted = first_cipher
            .encrypt_token("refresh-token-value")
            .expect("token should encrypt");
        let decrypted = second_cipher
            .decrypt_token(&encrypted)
            .expect("token should decrypt");

        assert_eq!(decrypted, "refresh-token-value");
    }

    #[test]
    fn encrypting_on_fresh_install_creates_credential_key() {
        let secret_store = MemorySecretStore::default();
        let cipher = AccountTokenCipher::new(secret_store.clone());

        let encrypted = cipher
            .encrypt_token("refresh-token-value")
            .expect("fresh install should create a credential key");

        assert_eq!(
            cipher
                .decrypt_token(&encrypted)
                .expect("created key should decrypt the token"),
            "refresh-token-value"
        );
        assert_eq!(
            secret_store
                .values
                .lock()
                .expect("secret store mutex poisoned")
                .len(),
            1
        );
    }

    #[test]
    fn missing_credential_key_does_not_orphan_existing_ciphertext() {
        let secret_store = MemorySecretStore::default();
        let encrypted = AccountTokenCipher::new(secret_store.clone())
            .encrypt_token("refresh-token-value")
            .expect("token should encrypt");
        secret_store
            .values
            .lock()
            .expect("secret store mutex poisoned")
            .clear();

        let cipher = AccountTokenCipher::new(secret_store.clone());
        let error = cipher
            .decrypt_token(&encrypted)
            .expect_err("missing key must not be silently replaced");

        assert!(error
            .to_string()
            .contains("saved accounts cannot be decrypted"));
        let encryption_error = cipher
            .encrypt_token("replacement-token")
            .expect_err("cipher must remember that saved ciphertext lost its key");
        assert!(encryption_error
            .to_string()
            .contains("saved accounts cannot be decrypted"));
        assert!(
            secret_store
                .values
                .lock()
                .expect("secret store mutex poisoned")
                .is_empty(),
            "decrypting existing ciphertext must not create a replacement key"
        );
    }

    #[test]
    fn encrypted_repository_refuses_replacement_key_for_saved_accounts() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let secret_store = MemorySecretStore::default();
        let repository =
            EncryptedAccountsRepository::new(&connection, secret_store.clone());
        let account = PlaintextAccountRecord {
            microsoft_id: "account-a".into(),
            xbox_gamertag: Some("PlayerA".into()),
            minecraft_uuid: Some("uuid-a".into()),
            access_token: Some("access-before".into()),
            refresh_token: Some("refresh-before".into()),
            profile_data: None,
            is_active: true,
        };
        repository
            .upsert_account(&account)
            .expect("initial account should store");
        let original_blob: Vec<u8> = connection
            .query_row(
                "SELECT refresh_token_enc FROM accounts WHERE microsoft_id = ?1",
                ["account-a"],
                |row| row.get(0),
            )
            .expect("stored blob should load");
        secret_store
            .values
            .lock()
            .expect("secret store mutex poisoned")
            .clear();

        let error = repository
            .upsert_account(&PlaintextAccountRecord {
                access_token: Some("access-after".into()),
                refresh_token: Some("refresh-after".into()),
                ..account
            })
            .expect_err("saved ciphertext must prevent replacement-key creation");

        assert!(error
            .to_string()
            .contains("saved accounts cannot be decrypted"));
        assert!(
            secret_store
                .values
                .lock()
                .expect("secret store mutex poisoned")
                .is_empty(),
            "failed upsert must not create a replacement key"
        );
        let unchanged_blob: Vec<u8> = connection
            .query_row(
                "SELECT refresh_token_enc FROM accounts WHERE microsoft_id = ?1",
                ["account-a"],
                |row| row.get(0),
            )
            .expect("stored blob should remain");
        assert_eq!(unchanged_blob, original_blob);

        drop(repository);
        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn encrypted_repository_persists_ciphertext_and_loads_plaintext() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");

        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let repository =
            EncryptedAccountsRepository::new(&connection, MemorySecretStore::default());

        repository
            .upsert_account(&PlaintextAccountRecord {
                microsoft_id: "account-a".into(),
                xbox_gamertag: Some("PlayerA".into()),
                minecraft_uuid: Some("uuid-a".into()),
                access_token: Some("access-a".into()),
                refresh_token: Some("refresh-a".into()),
                profile_data: Some("{\"name\":\"PlayerA\"}".into()),
                is_active: true,
            })
            .expect("account should store");

        let raw_row = connection
            .query_row(
                "SELECT access_token_enc, refresh_token_enc FROM accounts WHERE microsoft_id = ?1",
                ["account-a"],
                |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                    ))
                },
            )
            .expect("stored row should exist");
        let active_account = repository
            .load_active_account()
            .expect("active account should load")
            .expect("active account should exist");

        assert!(raw_row.0.is_some());
        assert!(raw_row.1.is_some());
        assert_ne!(raw_row.0.unwrap(), b"access-a");
        assert_ne!(raw_row.1.unwrap(), b"refresh-a");
        assert_eq!(active_account.access_token.as_deref(), Some("access-a"));
        assert_eq!(active_account.refresh_token.as_deref(), Some("refresh-a"));
        assert_eq!(active_account.xbox_gamertag.as_deref(), Some("PlayerA"));

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }
    #[test]
    fn list_accounts_skips_corrupted_blob() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");

        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let repository =
            EncryptedAccountsRepository::new(&connection, MemorySecretStore::default());

        repository
            .upsert_account(&PlaintextAccountRecord {
                microsoft_id: "account-valid".into(),
                xbox_gamertag: Some("ValidPlayer".into()),
                minecraft_uuid: Some("valid-uuid".into()),
                access_token: Some("valid-access-token".into()),
                refresh_token: Some("valid-refresh-token".into()),
                profile_data: None,
                is_active: false,
            })
            .expect("valid account should store");

        let mut corrupted_payload = vec![TOKEN_ENCRYPTION_VERSION];
        corrupted_payload.extend_from_slice(&[0; TOKEN_NONCE_LENGTH + 16]);
        connection
            .execute(
                r#"
                INSERT INTO accounts (
                    microsoft_id,
                    access_token_enc,
                    last_login,
                    is_active
                ) VALUES (?1, ?2, CURRENT_TIMESTAMP, FALSE)
                "#,
                rusqlite::params!["account-corrupted", corrupted_payload],
            )
            .expect("corrupted account should store");

        let accounts = repository
            .list_accounts()
            .expect("corrupted account should be skipped");

        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].microsoft_id, "account-valid");

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    fn player(microsoft_id: &str, token_suffix: &str, is_active: bool) -> PlaintextAccountRecord {
        PlaintextAccountRecord {
            microsoft_id: microsoft_id.into(),
            xbox_gamertag: Some(format!("{microsoft_id}-tag")),
            minecraft_uuid: Some(format!("{microsoft_id}-uuid")),
            access_token: Some(format!("access-{token_suffix}")),
            refresh_token: Some(format!("refresh-{token_suffix}")),
            profile_data: None,
            is_active,
        }
    }

    fn blobs(connection: &Connection) -> Vec<(String, Option<Vec<u8>>, Option<Vec<u8>>)> {
        let mut statement = connection
            .prepare(
                "SELECT microsoft_id, access_token_enc, refresh_token_enc FROM accounts ORDER BY microsoft_id",
            )
            .expect("query should prepare");
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .expect("query should run")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("rows should load")
    }

    /// F1: the key vanished (the keyring answers "no such entry") while two
    /// accounts still hold blobs. An ordinary save keeps refusing; only the
    /// explicit "Sign in again" discards the unreadable blobs, and it keeps
    /// every account row.
    #[test]
    fn sign_in_again_replaces_unreadable_blobs_and_keeps_the_accounts() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let secret_store = MemorySecretStore::default();
        let repository = EncryptedAccountsRepository::new(&connection, secret_store.clone());
        repository
            .upsert_account(&player("account-a", "before", true))
            .expect("first account should store");
        repository
            .upsert_account(&player("account-b", "before", false))
            .expect("second account should store");
        let blobs_before = blobs(&connection);
        secret_store
            .values
            .lock()
            .expect("secret store mutex poisoned")
            .clear();

        assert!(repository.check_login_can_be_saved(false).is_err());
        assert!(repository
            .save_login(&player("account-a", "after", true), false)
            .is_err());
        assert_eq!(blobs(&connection), blobs_before);
        assert!(secret_store
            .values
            .lock()
            .expect("secret store mutex poisoned")
            .is_empty());

        repository
            .check_login_can_be_saved(true)
            .expect("sign in again should be allowed");
        repository
            .save_login(&player("account-a", "after", true), true)
            .expect("sign in again should save");

        let blobs_after = blobs(&connection);
        assert_eq!(blobs_after.len(), 2, "no account row may disappear");
        assert_eq!(blobs_after[1], ("account-b".to_string(), None, None));
        let reload = || {
            EncryptedAccountsRepository::new(&connection, secret_store.clone())
                .load_active_account()
                .expect("active account should load")
                .expect("active account should exist")
        };
        let active = reload();
        assert_eq!(active.microsoft_id, "account-a");
        assert_eq!(active.access_token.as_deref(), Some("access-after"));
        assert_eq!(active.refresh_token.as_deref(), Some("refresh-after"));

        // With the key back, even the gesture must not rotate it: a new key
        // at every sign-in would orphan the blobs of every other account.
        let key_after_replace = secret_store
            .values
            .lock()
            .expect("secret store mutex poisoned")
            .clone();
        repository
            .save_login(&player("account-b", "later", false), true)
            .expect("second sign in should save");
        assert_eq!(
            *secret_store.values.lock().expect("secret store mutex poisoned"),
            key_after_replace
        );
        assert_eq!(reload().refresh_token.as_deref(), Some("refresh-after"));

        drop(repository);
        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// F1: the save guard counts both columns, so a lone access blob that
    /// nobody can decrypt must read as `Unreadable` (the state that offers
    /// the way out), not as `SignedOut`.
    #[test]
    fn a_lone_unreadable_access_blob_is_not_signed_out() {
        let secret_store = MemorySecretStore::default();
        let cipher = AccountTokenCipher::new(secret_store.clone());
        let access = cipher.encrypt_token("access").expect("token should encrypt");

        assert_eq!(cipher.credential_state(None, Some(&access)), CredentialState::SignedOut);
        secret_store
            .values
            .lock()
            .expect("secret store mutex poisoned")
            .clear();
        assert_eq!(cipher.credential_state(None, Some(&access)), CredentialState::Unreadable);
        assert_eq!(cipher.credential_state(None, None), CredentialState::SignedOut);
    }

    /// F1: a keyring that cannot be asked says nothing about the blobs, so
    /// even the explicit gesture must not discard them.
    #[test]
    fn sign_in_again_discards_nothing_when_the_keyring_cannot_be_asked() {
        struct UnreachableSecretStore;

        impl SecretStore for UnreachableSecretStore {
            fn get_secret(&self, _key: &str) -> anyhow::Result<Option<String>> {
                Err(anyhow::anyhow!("no secret service"))
            }

            fn set_secret(&self, _key: &str, _secret: &str) -> anyhow::Result<()> {
                Err(anyhow::anyhow!("no secret service"))
            }
        }

        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        EncryptedAccountsRepository::new(&connection, MemorySecretStore::default())
            .upsert_account(&player("account-a", "before", true))
            .expect("account should store");
        let blobs_before = blobs(&connection);

        let repository = EncryptedAccountsRepository::new(&connection, UnreachableSecretStore);
        assert!(repository.check_login_can_be_saved(true).is_err());
        assert!(repository
            .save_login(&player("account-a", "after", true), true)
            .is_err());
        assert_eq!(blobs(&connection), blobs_before);

        drop(repository);
        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

}
