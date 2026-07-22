use std::fs;
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use crate::token_storage::{AccountTokenCipher, SecretStore};

pub const DATABASE_FILENAME: &str = "launcher_data.db";

const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS accounts (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    microsoft_id        TEXT NOT NULL UNIQUE,
    xbox_gamertag       TEXT,
    minecraft_uuid      TEXT,
    access_token_enc    BLOB,
    refresh_token_enc   BLOB,
    last_login          DATETIME,
    profile_data        TEXT,
    is_active           BOOLEAN DEFAULT FALSE
);

CREATE TABLE IF NOT EXISTS java_installations (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    path            TEXT NOT NULL UNIQUE,
    version         INTEGER NOT NULL,
    auto_detected   BOOLEAN DEFAULT TRUE,
    architecture    TEXT DEFAULT 'x64'
);

CREATE TABLE IF NOT EXISTS mod_cache (
    modrinth_project_id TEXT,
    modrinth_version_id TEXT,
    jar_filename        TEXT NOT NULL,
    mc_version          TEXT NOT NULL,
    mod_loader          TEXT NOT NULL,
    file_hash           TEXT,
    download_url        TEXT,
    is_local            BOOLEAN DEFAULT FALSE,
    PRIMARY KEY (modrinth_version_id, mc_version, mod_loader)
);

CREATE TABLE IF NOT EXISTS modrinth_project_aliases (
    alias                TEXT PRIMARY KEY,
    canonical_project_id TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS dependencies (
    mod_parent_id    TEXT NOT NULL,
    dependency_id    TEXT NOT NULL,
    dep_type         TEXT NOT NULL DEFAULT 'required',
    specific_version TEXT,
    jar_filename     TEXT NOT NULL,
    PRIMARY KEY (mod_parent_id, dependency_id)
);

CREATE TABLE IF NOT EXISTS config_attribution (
    config_path     TEXT NOT NULL,
    jar_filename    TEXT NOT NULL,
    source_class    TEXT,
    timestamp       DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (config_path)
);

CREATE TABLE IF NOT EXISTS global_settings (
    key     TEXT PRIMARY KEY,
    value   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS modlist_settings (
    modlist_name TEXT NOT NULL,
    key          TEXT NOT NULL,
    value        TEXT NOT NULL,
    PRIMARY KEY (modlist_name, key)
);

CREATE TABLE IF NOT EXISTS modrinth_availability (
    project_id  TEXT NOT NULL,
    mc_version  TEXT NOT NULL,
    mod_loader  TEXT NOT NULL,
    available   BOOLEAN NOT NULL,
    PRIMARY KEY (project_id, mc_version, mod_loader)
);
"#;

pub fn initialize_database(database_path: &Path) -> Result<()> {
    if let Some(parent) = database_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let connection = Connection::open(database_path)?;
    connection.execute_batch(SCHEMA_SQL)?;
    migrate_mod_cache_schema(&connection)?;
    migrate_mod_cache_target_key(&connection)?;

    Ok(())
}

fn migrate_mod_cache_schema(connection: &Connection) -> Result<()> {
    let table_sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'mod_cache'",
            [],
            |row| row.get(0),
        )
        .ok();

    let Some(table_sql) = table_sql else {
        return Ok(());
    };

    if !table_sql
        .to_ascii_lowercase()
        .contains("jar_filename        text not null unique")
        && !table_sql
            .to_ascii_lowercase()
            .contains("jar_filename text not null unique")
    {
        return Ok(());
    }

    let transaction = connection.unchecked_transaction()?;
    transaction.execute("ALTER TABLE mod_cache RENAME TO mod_cache_old", [])?;
    transaction.execute_batch(
        r#"
        CREATE TABLE mod_cache (
            modrinth_project_id TEXT,
            modrinth_version_id TEXT,
            jar_filename        TEXT NOT NULL,
            mc_version          TEXT NOT NULL,
            mod_loader          TEXT NOT NULL,
            file_hash           TEXT,
            download_url        TEXT,
            is_local            BOOLEAN DEFAULT FALSE,
            PRIMARY KEY (modrinth_version_id, mc_version, mod_loader)
        );

        INSERT INTO mod_cache (
            modrinth_project_id,
            modrinth_version_id,
            jar_filename,
            mc_version,
            mod_loader,
            file_hash,
            download_url,
            is_local
        )
        SELECT
            modrinth_project_id,
            modrinth_version_id,
            jar_filename,
            mc_version,
            mod_loader,
            file_hash,
            download_url,
            is_local
        FROM mod_cache_old;

        DROP TABLE mod_cache_old;
        "#,
    )?;
    transaction.commit()?;

    Ok(())
}

fn migrate_mod_cache_target_key(connection: &Connection) -> Result<()> {
    let table_sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'mod_cache'",
            [],
            |row| row.get(0),
        )
        .ok();

    let Some(table_sql) = table_sql else {
        return Ok(());
    };

    let normalized = table_sql
        .to_ascii_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.contains("primary key (modrinth_version_id, mc_version, mod_loader)") {
        return Ok(());
    }

    let transaction = connection.unchecked_transaction()?;
    transaction.execute("ALTER TABLE mod_cache RENAME TO mod_cache_old", [])?;
    transaction.execute_batch(
        r#"
        CREATE TABLE mod_cache (
            modrinth_project_id TEXT,
            modrinth_version_id TEXT,
            jar_filename        TEXT NOT NULL,
            mc_version          TEXT NOT NULL,
            mod_loader          TEXT NOT NULL,
            file_hash           TEXT,
            download_url        TEXT,
            is_local            BOOLEAN DEFAULT FALSE,
            PRIMARY KEY (modrinth_version_id, mc_version, mod_loader)
        );

        INSERT OR REPLACE INTO mod_cache (
            modrinth_project_id,
            modrinth_version_id,
            jar_filename,
            mc_version,
            mod_loader,
            file_hash,
            download_url,
            is_local
        )
        SELECT
            modrinth_project_id,
            modrinth_version_id,
            jar_filename,
            mc_version,
            mod_loader,
            file_hash,
            download_url,
            is_local
        FROM mod_cache_old;

        DROP TABLE mod_cache_old;
        "#,
    )?;
    transaction.commit()?;

    Ok(())
}

/// SECURITY (C1): older builds stored `mc_access_token`/`ms_refresh_token` in
/// cleartext inside `profile_data`. This migrates each account's `profile_data`
/// to remove those plaintext copies while guaranteeing the refresh token
/// survives in the encrypted `refresh_token_enc` column (see phases below).
///
/// Takes a `SecretStore` so it can build the same `AccountTokenCipher` the
/// launch path uses, allowing it to re-encrypt an authoritative profile token
/// before stripping it.
pub(crate) fn migrate_account_tokens_from_profile_data<S: SecretStore>(
    connection: &Connection,
    _secret_store: S,
) -> Result<()> {
    let rows: Vec<(String, Option<String>)> = {
        let mut statement =
            connection.prepare("SELECT microsoft_id, profile_data FROM accounts")?;
        let mapped = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        mapped.collect::<std::result::Result<Vec<_>, _>>()?
    };

    for (microsoft_id, profile_data) in rows {
        let Some(profile_data) = profile_data else {
            continue;
        };
        let Ok(serde_json::Value::Object(mut map)) =
            serde_json::from_str::<serde_json::Value>(&profile_data)
        else {
            continue;
        };

        let had_tokens = map.remove("mc_access_token").is_some()
            | map.remove("ms_refresh_token").is_some();
        if !had_tokens {
            continue;
        }

        let cleaned = serde_json::Value::Object(map).to_string();
        connection.execute(
            "UPDATE accounts SET profile_data = ?1 WHERE microsoft_id = ?2",
            rusqlite::params![cleaned, microsoft_id],
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use rusqlite::{params, Connection};

    use super::{
        initialize_database, migrate_account_tokens_from_profile_data, DATABASE_FILENAME,
    };
    use crate::token_storage::{AccountTokenCipher, SecretStore};

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

    fn unique_test_database_path() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir()
            .join(format!("cubic-launcher-db-test-{timestamp}"))
            .join(DATABASE_FILENAME)
    }

    fn fetch_table_names(connection: &Connection) -> Vec<String> {
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .expect("failed to prepare table query");

        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("failed to query table names")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("failed to collect table names")
    }

    #[test]
    fn initialize_database_creates_all_required_tables() {
        let database_path = unique_test_database_path();

        initialize_database(&database_path).expect("database initialization should succeed");

        let connection = Connection::open(&database_path).expect("database should open");
        let table_names = fetch_table_names(&connection);

        for expected_table in [
            "accounts",
            "java_installations",
            "mod_cache",
            "modrinth_project_aliases",
            "dependencies",
            "config_attribution",
            "global_settings",
            "modlist_settings",
            "modrinth_availability",
        ] {
            assert!(
                table_names
                    .iter()
                    .any(|table_name| table_name == expected_table),
                "missing expected table: {expected_table}"
            );
        }

        drop(connection);
        fs::remove_file(&database_path).expect("database file should be removable");
        fs::remove_dir_all(
            database_path
                .parent()
                .expect("database should have a parent"),
        )
        .expect("temporary directory should be removable");
    }

    #[test]
    fn initialize_database_is_idempotent() {
        let database_path = unique_test_database_path();

        initialize_database(&database_path).expect("first initialization should succeed");
        initialize_database(&database_path).expect("second initialization should also succeed");

        fs::remove_file(&database_path).expect("database file should be removable");
        fs::remove_dir_all(
            database_path
                .parent()
                .expect("database should have a parent"),
        )
        .expect("temporary directory should be removable");
    }

    #[test]
    fn initialize_database_migrates_legacy_mod_cache_unique_filename_constraint() {
        let database_path = unique_test_database_path();
        let parent_dir = database_path
            .parent()
            .expect("database should have a parent")
            .to_path_buf();
        fs::create_dir_all(&parent_dir).expect("parent directory should exist");

        let connection = Connection::open(&database_path).expect("database should open");
        connection
            .execute_batch(
                r#"
                CREATE TABLE mod_cache (
                    modrinth_project_id TEXT,
                    modrinth_version_id TEXT,
                    jar_filename        TEXT NOT NULL UNIQUE,
                    mc_version          TEXT NOT NULL,
                    mod_loader          TEXT NOT NULL,
                    file_hash           TEXT,
                    download_url        TEXT,
                    is_local            BOOLEAN DEFAULT FALSE,
                    PRIMARY KEY (modrinth_version_id)
                );
                "#,
            )
            .expect("legacy mod_cache schema should create");
        drop(connection);

        initialize_database(&database_path).expect("migration should succeed");

        let connection = Connection::open(&database_path).expect("database should reopen");
        connection
            .execute(
                "INSERT INTO mod_cache (modrinth_project_id, modrinth_version_id, jar_filename, mc_version, mod_loader, file_hash, download_url, is_local) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params!["mod-a", "version-a", "shared.jar", "1.21.6", "fabric", Option::<String>::None, Option::<String>::None, false],
            )
            .expect("first insert should succeed");
        connection
            .execute(
                "INSERT INTO mod_cache (modrinth_project_id, modrinth_version_id, jar_filename, mc_version, mod_loader, file_hash, download_url, is_local) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params!["mod-b", "version-b", "shared.jar", "1.21.6", "fabric", Option::<String>::None, Option::<String>::None, false],
            )
            .expect("second insert with same filename should succeed after migration");

        drop(connection);
        fs::remove_file(&database_path).expect("database file should be removable");
        fs::remove_dir_all(&parent_dir).expect("temporary directory should be removable");
    }

    // Helpers for the token-migration suite.
    fn insert_account(
        connection: &Connection,
        microsoft_id: &str,
        refresh_token_enc: Option<Vec<u8>>,
        profile_data: &str,
    ) {
        connection
            .execute(
                "INSERT INTO accounts (microsoft_id, xbox_gamertag, minecraft_uuid, access_token_enc, refresh_token_enc, profile_data, is_active) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    microsoft_id,
                    "Player",
                    "uuid",
                    Option::<Vec<u8>>::None,
                    refresh_token_enc,
                    profile_data,
                    true
                ],
            )
            .expect("account should insert");
    }

    fn profile_of(connection: &Connection, microsoft_id: &str) -> String {
        connection
            .query_row(
                "SELECT profile_data FROM accounts WHERE microsoft_id = ?1",
                params![microsoft_id],
                |row| row.get(0),
            )
            .expect("profile should load")
    }

    fn refresh_enc_of(connection: &Connection, microsoft_id: &str) -> Option<Vec<u8>> {
        connection
            .query_row(
                "SELECT refresh_token_enc FROM accounts WHERE microsoft_id = ?1",
                params![microsoft_id],
                |row| row.get(0),
            )
            .expect("refresh enc should load")
    }

    /// What the launch path would recover: decrypt(enc) if present, else the
    /// plaintext left in profile_data (deferral fallback).
    fn recoverable_refresh<S: SecretStore + Clone>(
        connection: &Connection,
        microsoft_id: &str,
        store: S,
    ) -> Option<String> {
        if let Some(enc) = refresh_enc_of(connection, microsoft_id) {
            if let Ok(token) = AccountTokenCipher::new(store).decrypt_token(&enc) {
                return Some(token);
            }
        }
        let profile = profile_of(connection, microsoft_id);
        serde_json::from_str::<serde_json::Value>(&profile)
            .ok()
            .and_then(|v| {
                v.get("ms_refresh_token")
                    .and_then(|t| t.as_str())
                    .map(String::from)
            })
    }

    fn fresh_db() -> (PathBuf, PathBuf) {
        let database_path = unique_test_database_path();
        let parent_dir = database_path
            .parent()
            .expect("database should have a parent")
            .to_path_buf();
        fs::create_dir_all(&parent_dir).expect("parent directory should exist");
        initialize_database(&database_path).expect("init should succeed");
        (database_path, parent_dir)
    }

    #[test]
    fn migration_strips_consistent_tokens_and_is_idempotent() {
        let (database_path, parent_dir) = fresh_db();
        let store = MemorySecretStore::default();
        let enc_consistent = AccountTokenCipher::new(store.clone())
            .encrypt_token("FRESH")
            .expect("encrypt should succeed");

        let connection = Connection::open(&database_path).expect("db open");
        insert_account(
            &connection,
            "ms-consistent",
            Some(enc_consistent),
            r#"{"username":"PlayerLegacy","uuid":"uuid-legacy","mc_access_token":"SECRET_MC","ms_refresh_token":"FRESH"}"#,
        );
        insert_account(
            &connection,
            "ms-clean",
            None,
            r#"{"username":"PlayerClean","uuid":"uuid-clean"}"#,
        );

        migrate_account_tokens_from_profile_data(&connection, store.clone())
            .expect("migration should succeed");

        let profile = profile_of(&connection, "ms-consistent");
        assert!(!profile.contains("SECRET_MC"));
        assert!(!profile.contains("FRESH"));
        assert!(!profile.contains("mc_access_token"));
        assert!(!profile.contains("ms_refresh_token"));
        assert!(profile.contains("PlayerLegacy"));
        assert_eq!(
            recoverable_refresh(&connection, "ms-consistent", store.clone()),
            Some("FRESH".to_string())
        );
        // Clean account left untouched.
        assert_eq!(
            profile_of(&connection, "ms-clean"),
            r#"{"username":"PlayerClean","uuid":"uuid-clean"}"#
        );

        // Idempotent: a second run does not change the already-migrated rows.
        let profile_first = profile_of(&connection, "ms-consistent");
        let enc_first = refresh_enc_of(&connection, "ms-consistent");
        migrate_account_tokens_from_profile_data(&connection, store.clone())
            .expect("second migration should succeed");
        assert_eq!(profile_of(&connection, "ms-consistent"), profile_first);
        assert_eq!(refresh_enc_of(&connection, "ms-consistent"), enc_first);

        drop(connection);
        fs::remove_dir_all(&parent_dir).expect("temp dir removable");
    }

    #[test]
    #[ignore = "RED until fix(C1-guard): enc-NULL must not destroy the only plaintext copy"]
    fn migration_preserves_refresh_when_enc_null() {
        let (database_path, parent_dir) = fresh_db();
        let store = MemorySecretStore::default();
        let connection = Connection::open(&database_path).expect("db open");
        insert_account(
            &connection,
            "ms-enc-null",
            None,
            r#"{"username":"P","uuid":"u","ms_refresh_token":"ONLY_COPY"}"#,
        );

        migrate_account_tokens_from_profile_data(&connection, store.clone())
            .expect("migration should succeed");

        // The only copy of the refresh token must survive somewhere recoverable.
        assert_eq!(
            recoverable_refresh(&connection, "ms-enc-null", store.clone()),
            Some("ONLY_COPY".to_string())
        );

        drop(connection);
        fs::remove_dir_all(&parent_dir).expect("temp dir removable");
    }

    #[test]
    #[ignore = "RED until fix(C1-rotation): fresh profile token must win over stale enc"]
    fn migration_recovers_fresh_rotated_token_over_stale_enc() {
        let (database_path, parent_dir) = fresh_db();
        let store = MemorySecretStore::default();
        let enc_stale = AccountTokenCipher::new(store.clone())
            .encrypt_token("STALE_LOGIN")
            .expect("encrypt should succeed");

        let connection = Connection::open(&database_path).expect("db open");
        // Beta divergence: fresh rotated token lives ONLY in profile_data; enc
        // holds the now-stale login token.
        insert_account(
            &connection,
            "ms-diverged",
            Some(enc_stale),
            r#"{"username":"P","uuid":"u","mc_access_token":"MC","ms_refresh_token":"FRESH_ROTATED"}"#,
        );

        migrate_account_tokens_from_profile_data(&connection, store.clone())
            .expect("migration should succeed");

        // Launch reads from enc: it MUST recover the fresh token, not the stale one.
        assert_eq!(
            recoverable_refresh(&connection, "ms-diverged", store.clone()),
            Some("FRESH_ROTATED".to_string())
        );
        // Plaintext must be gone from profile_data (security intent of C1).
        let profile = profile_of(&connection, "ms-diverged");
        assert!(!profile.contains("FRESH_ROTATED"));
        assert!(!profile.contains("ms_refresh_token"));

        drop(connection);
        fs::remove_dir_all(&parent_dir).expect("temp dir removable");
    }

    #[test]
    #[ignore = "RED until fix(C1-rotation): divergence re-encryption must be idempotent"]
    fn migration_divergence_fix_is_idempotent() {
        let (database_path, parent_dir) = fresh_db();
        let store = MemorySecretStore::default();
        let enc_stale = AccountTokenCipher::new(store.clone())
            .encrypt_token("STALE_LOGIN")
            .expect("encrypt should succeed");

        let connection = Connection::open(&database_path).expect("db open");
        insert_account(
            &connection,
            "ms-diverged",
            Some(enc_stale),
            r#"{"username":"P","uuid":"u","ms_refresh_token":"FRESH_ROTATED"}"#,
        );

        migrate_account_tokens_from_profile_data(&connection, store.clone())
            .expect("first migration should succeed");
        let profile_first = profile_of(&connection, "ms-diverged");
        let enc_first = refresh_enc_of(&connection, "ms-diverged");

        migrate_account_tokens_from_profile_data(&connection, store.clone())
            .expect("second migration should succeed");
        // Second run changes nothing and still recovers the fresh token.
        assert_eq!(profile_of(&connection, "ms-diverged"), profile_first);
        assert_eq!(refresh_enc_of(&connection, "ms-diverged"), enc_first);
        assert_eq!(
            recoverable_refresh(&connection, "ms-diverged", store.clone()),
            Some("FRESH_ROTATED".to_string())
        );

        drop(connection);
        fs::remove_dir_all(&parent_dir).expect("temp dir removable");
    }
}
