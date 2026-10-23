use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::rngs::OsRng;
use rand::RngCore;
use reqwest::Url;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MICROSOFT_AUTHORIZE_URL: &str =
    "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize";
const MICROSOFT_TOKEN_URL: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0/token";
const BUNDLED_MICROSOFT_CLIENT_ID: &str = "7dce88aa-79b0-4b77-9666-0fdb8addd50c";

/// How long a sign-in or refresh request waits for a connection to open.
const AUTH_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// How long one request of the sign-in and refresh chain may take, body
/// included: every answer is a small JSON document.
const AUTH_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The client of the Microsoft, Xbox and Minecraft services. Before audit 101
/// (account-7) it was `reqwest::Client::new()`, with no timeout at all: a
/// service that accepted the connection and said nothing kept the launch on
/// «Authenticating» for as long as the operating system allowed.
fn auth_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(AUTH_CONNECT_TIMEOUT)
        .timeout(AUTH_REQUEST_TIMEOUT)
        .build()
        .unwrap_or_default()
}

/// The token endpoint refused a request, with the OAuth `error` code of its
/// answer when it had one. Only the code: `error_description` carries trace
/// ids and timestamps, never a token, but nothing here needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrosoftTokenEndpointError {
    pub status: u16,
    pub error: Option<String>,
}

impl std::fmt::Display for MicrosoftTokenEndpointError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.error {
            Some(error) => write!(formatter, "HTTP status {} ({error})", self.status),
            None => write!(formatter, "HTTP status {}", self.status),
        }
    }
}

impl std::error::Error for MicrosoftTokenEndpointError {}

/// D143 (audit 101, account-1): whether a failed refresh means the saved
/// sign-in is dead and only a new interactive sign-in can bring it back.
/// `invalid_grant` is the revoked or expired refresh token (password change,
/// ~90 days unused); `interaction_required` is Microsoft asking the user
/// to sign in themselves. A network error, a 5xx or a refusal of the client
/// itself (`invalid_client`) say nothing about the account and keep it.
pub fn refresh_needs_sign_in_again(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<MicrosoftTokenEndpointError>()
            .is_some_and(|error| {
                matches!(
                    error.error.as_deref(),
                    Some("invalid_grant") | Some("interaction_required")
                )
            })
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrosoftOAuthConfig {
    pub client_id: String,
    pub redirect_uri: String,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrosoftOAuthSession {
    pub state: String,
    pub code_verifier: String,
    pub code_challenge: String,
    pub authorization_url: Url,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationCallback {
    pub code: String,
    pub state: String,
}

#[derive(Debug, Clone)]
pub struct MicrosoftOAuthClient {
    http_client: reqwest::Client,
    authorize_url: String,
    token_url: String,
}

impl MicrosoftOAuthClient {
    pub fn new() -> Self {
        Self {
            http_client: auth_http_client(),
            authorize_url: MICROSOFT_AUTHORIZE_URL.to_string(),
            token_url: MICROSOFT_TOKEN_URL.to_string(),
        }
    }

    /// Test seam: the same client against a local token endpoint.
    #[cfg(test)]
    pub fn with_endpoints(authorize_url: impl Into<String>, token_url: impl Into<String>) -> Self {
        Self {
            http_client: auth_http_client(),
            authorize_url: authorize_url.into(),
            token_url: token_url.into(),
        }
    }

    pub fn start_session(&self, config: &MicrosoftOAuthConfig) -> Result<MicrosoftOAuthSession> {
        validate_oauth_config(config)?;

        let state = generate_url_safe_random_bytes(16);
        let code_verifier = generate_url_safe_random_bytes(32);
        let code_challenge = build_pkce_code_challenge(&code_verifier);
        let authorization_url =
            build_authorization_url(&self.authorize_url, config, &state, &code_challenge)?;

        Ok(MicrosoftOAuthSession {
            state,
            code_verifier,
            code_challenge,
            authorization_url,
        })
    }

    pub async fn exchange_authorization_code(
        &self,
        config: &MicrosoftOAuthConfig,
        callback: &AuthorizationCallback,
        code_verifier: &str,
    ) -> Result<MicrosoftTokenResponse> {
        validate_oauth_config(config)?;

        let response = self
            .http_client
            .post(&self.token_url)
            .form(&[
                ("client_id", config.client_id.as_str()),
                ("redirect_uri", config.redirect_uri.as_str()),
                ("grant_type", "authorization_code"),
                ("code", callback.code.as_str()),
                ("code_verifier", code_verifier),
                ("scope", &join_scopes(&config.scopes)),
            ])
            .send()
            .await
            .context("failed to exchange Microsoft authorization code")?
            .error_for_status()
            .context("Microsoft token endpoint returned an error")?;

        response
            .json::<MicrosoftTokenResponse>()
            .await
            .context("failed to deserialize Microsoft token response")
    }

    pub async fn refresh_access_token(
        &self,
        config: &MicrosoftOAuthConfig,
        refresh_token: &str,
    ) -> Result<MicrosoftTokenResponse> {
        validate_oauth_config(config)?;

        let response = self
            .http_client
            .post(&self.token_url)
            .form(&[
                ("client_id", config.client_id.as_str()),
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh_token),
                ("scope", &join_scopes(&config.scopes)),
            ])
            .send()
            .await
            .context("failed to refresh Microsoft access token")?;
        let status = response.status();
        if !status.is_success() {
            #[derive(Deserialize)]
            struct TokenErrorBody {
                error: Option<String>,
            }
            let error = response
                .json::<TokenErrorBody>()
                .await
                .ok()
                .and_then(|body| body.error);
            return Err(anyhow::Error::new(MicrosoftTokenEndpointError {
                status: status.as_u16(),
                error,
            })
            .context("Microsoft refresh token endpoint returned an error"));
        }

        response
            .json::<MicrosoftTokenResponse>()
            .await
            .context("failed to deserialize Microsoft refresh response")
    }
}

impl Default for MicrosoftOAuthClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MicrosoftTokenResponse {
    pub token_type: String,
    pub scope: Option<String>,
    pub expires_in: u64,
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRecord {
    pub microsoft_id: String,
    pub xbox_gamertag: Option<String>,
    pub minecraft_uuid: Option<String>,
    pub access_token_enc: Option<Vec<u8>>,
    pub refresh_token_enc: Option<Vec<u8>>,
    pub profile_data: Option<String>,
    pub is_active: bool,
    /// The user's choice to play this account offline (D115). Read from the
    /// row; written only by `set_plays_offline`: `upsert_account` saves a
    /// sign-in and leaves the choice as it was, so this field is ignored there.
    pub plays_offline: bool,
}

/// The columns `AccountsRepository::update_session_tokens` writes. The refresh
/// token always; `None` in any other field keeps what the row has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTokenUpdate {
    pub refresh_token_enc: Vec<u8>,
    pub access_token_enc: Option<Vec<u8>>,
    pub minecraft_uuid: Option<String>,
    pub profile_data: Option<String>,
}

pub struct AccountsRepository<'connection> {
    connection: &'connection Connection,
}

impl<'connection> AccountsRepository<'connection> {
    pub fn new(connection: &'connection Connection) -> Self {
        Self { connection }
    }

    pub fn upsert_account(&self, account: &AccountRecord) -> Result<()> {
        self.connection.execute(
            r#"
            INSERT INTO accounts (
                microsoft_id,
                xbox_gamertag,
                minecraft_uuid,
                access_token_enc,
                refresh_token_enc,
                last_login,
                profile_data,
                is_active
            ) VALUES (?1, ?2, ?3, ?4, ?5, CURRENT_TIMESTAMP, ?6, ?7)
            ON CONFLICT(microsoft_id) DO UPDATE SET
                xbox_gamertag = excluded.xbox_gamertag,
                minecraft_uuid = excluded.minecraft_uuid,
                access_token_enc = excluded.access_token_enc,
                refresh_token_enc = excluded.refresh_token_enc,
                last_login = CURRENT_TIMESTAMP,
                profile_data = excluded.profile_data,
                is_active = excluded.is_active
            "#,
            params![
                &account.microsoft_id,
                &account.xbox_gamertag,
                &account.minecraft_uuid,
                &account.access_token_enc,
                &account.refresh_token_enc,
                &account.profile_data,
                account.is_active,
            ],
        )?;

        if account.is_active {
            self.set_active_account(&account.microsoft_id)?;
        }

        Ok(())
    }

    pub fn set_active_account(&self, microsoft_id: &str) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute("UPDATE accounts SET is_active = FALSE", [])?;
        let updated_rows = transaction.execute(
            "UPDATE accounts SET is_active = TRUE WHERE microsoft_id = ?1",
            [microsoft_id],
        )?;

        if updated_rows == 0 {
            bail!("account '{}' does not exist", microsoft_id);
        }

        transaction.commit()?;
        Ok(())
    }

    /// Saves the user's choice for one account (D115). Nothing else about the
    /// row changes: the sign-in and its tokens stay as they are.
    pub fn set_plays_offline(&self, microsoft_id: &str, plays_offline: bool) -> Result<()> {
        let updated_rows = self.connection.execute(
            "UPDATE accounts SET plays_offline = ?1 WHERE microsoft_id = ?2",
            params![plays_offline, microsoft_id],
        )?;
        if updated_rows == 0 {
            bail!("account '{}' does not exist", microsoft_id);
        }
        Ok(())
    }

    /// Deletes one saved account. When it was the active one and others are
    /// left, the most recently signed-in of them becomes active in the same
    /// transaction: the launch reads the active row (its sign-in and its
    /// «Play offline», D115), and a database with accounts but none active
    /// would launch with nobody while the interface shows the first row.
    pub fn delete_account(&self, microsoft_id: &str) -> Result<()> {
        let transaction = self.connection.unchecked_transaction()?;
        let was_active: Option<bool> = transaction
            .query_row(
                "SELECT is_active FROM accounts WHERE microsoft_id = ?1",
                [microsoft_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(was_active) = was_active else {
            bail!("account '{}' does not exist", microsoft_id);
        };
        transaction.execute(
            "DELETE FROM accounts WHERE microsoft_id = ?1",
            [microsoft_id],
        )?;
        if was_active {
            transaction.execute(
                "UPDATE accounts SET is_active = TRUE WHERE id = \
                 (SELECT id FROM accounts ORDER BY last_login DESC, id DESC LIMIT 1)",
                [],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn load_active_account(&self) -> Result<Option<AccountRecord>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT
                microsoft_id,
                xbox_gamertag,
                minecraft_uuid,
                access_token_enc,
                refresh_token_enc,
                profile_data,
                is_active,
                plays_offline
            FROM accounts
            WHERE is_active = TRUE
            LIMIT 1
            "#,
        )?;

        statement
            .query_row([], |row| {
                Ok(AccountRecord {
                    microsoft_id: row.get(0)?,
                    xbox_gamertag: row.get(1)?,
                    minecraft_uuid: row.get(2)?,
                    access_token_enc: row.get(3)?,
                    refresh_token_enc: row.get(4)?,
                    profile_data: row.get(5)?,
                    is_active: row.get(6)?,
                    plays_offline: row.get(7)?,
                })
            })
            .optional()
            .map_err(Into::into)
    }

    pub fn list_accounts(&self) -> Result<Vec<AccountRecord>> {
        let mut statement = self.connection.prepare(
            r#"
            SELECT
                microsoft_id,
                xbox_gamertag,
                minecraft_uuid,
                access_token_enc,
                refresh_token_enc,
                profile_data,
                is_active,
                plays_offline
            FROM accounts
            ORDER BY last_login DESC, id DESC
            "#,
        )?;

        let accounts = statement
            .query_map([], |row| {
                Ok(AccountRecord {
                    microsoft_id: row.get(0)?,
                    xbox_gamertag: row.get(1)?,
                    minecraft_uuid: row.get(2)?,
                    access_token_enc: row.get(3)?,
                    refresh_token_enc: row.get(4)?,
                    profile_data: row.get(5)?,
                    is_active: row.get(6)?,
                    plays_offline: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(anyhow::Error::from)?;

        Ok(accounts)
    }

    pub fn has_encrypted_account_tokens(&self) -> Result<bool> {
        self.connection
            .query_row(
                r#"
                SELECT EXISTS(
                    SELECT 1
                    FROM accounts
                    WHERE access_token_enc IS NOT NULL OR refresh_token_enc IS NOT NULL
                )
                "#,
                [],
                |row| row.get(0),
            )
            .map_err(Into::into)
    }

    /// Saves the tokens of a refresh made by the launch on the row it read at
    /// the start (concorrenza-8): the token blobs, and the identity the fresh
    /// sign-in reported, when there is one. `last_login` is not touched: it
    /// orders Manage Accounts by sign-in, and a launch is not one. Not an upsert: `is_active`, the
    /// play-offline choice and the other rows stay as they are, so an account
    /// switched away from, or removed, during the launch is not made active
    /// again or re-created. Returns whether the row still existed.
    pub fn update_session_tokens(
        &self,
        microsoft_id: &str,
        tokens: &SessionTokenUpdate,
    ) -> Result<bool> {
        let updated_rows = self.connection.execute(
            r#"
            UPDATE accounts SET
                access_token_enc = COALESCE(?2, access_token_enc),
                refresh_token_enc = ?3,
                minecraft_uuid = COALESCE(?4, minecraft_uuid),
                profile_data = COALESCE(?5, profile_data)
            WHERE microsoft_id = ?1
            "#,
            params![
                microsoft_id,
                &tokens.access_token_enc,
                &tokens.refresh_token_enc,
                &tokens.minecraft_uuid,
                &tokens.profile_data,
            ],
        )?;
        Ok(updated_rows > 0)
    }

    /// D143: Microsoft refused the saved refresh token for good, so it is
    /// dropped and the row reads as signed out, which is what offers «Sign in
    /// again». Only if the row still holds the blob the launch read: a sign-in
    /// saved in the meantime is not thrown away. Returns whether it cleared.
    pub fn clear_rejected_refresh_token(
        &self,
        microsoft_id: &str,
        rejected_refresh_token_enc: &[u8],
    ) -> Result<bool> {
        let updated_rows = self.connection.execute(
            "UPDATE accounts SET refresh_token_enc = NULL \
             WHERE microsoft_id = ?1 AND refresh_token_enc = ?2",
            params![microsoft_id, rejected_refresh_token_enc],
        )?;
        Ok(updated_rows > 0)
    }

    /// Sets every token blob to NULL and keeps the rows. Only the explicit
    /// "Sign in again" path calls it, when no blob can be decrypted any more.
    pub fn discard_encrypted_tokens(&self) -> Result<usize> {
        self.connection
            .execute(
                "UPDATE accounts SET access_token_enc = NULL, refresh_token_enc = NULL \
                 WHERE access_token_enc IS NOT NULL OR refresh_token_enc IS NOT NULL",
                [],
            )
            .map_err(Into::into)
    }
}

pub fn build_authorization_url(
    authorize_url: &str,
    config: &MicrosoftOAuthConfig,
    state: &str,
    code_challenge: &str,
) -> Result<Url> {
    validate_oauth_config(config)?;

    let mut url = Url::parse(authorize_url)
        .with_context(|| format!("invalid Microsoft authorize URL '{authorize_url}'"))?;

    url.query_pairs_mut()
        .append_pair("client_id", &config.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &config.redirect_uri)
        .append_pair("response_mode", "query")
        .append_pair("scope", &join_scopes(&config.scopes))
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        .append_pair("prompt", "select_account");

    Ok(url)
}

/// What one request on the loopback `/callback` means for the sign-in that
/// is waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CallbackOutcome {
    /// The authorization code of this sign-in.
    Code(AuthorizationCallback),
    /// This sign-in's answer, and it is not a code: the user refused, or
    /// Microsoft reported an error. The text says which.
    Failed(String),
    /// Not this sign-in's answer: the state is missing or another one. The
    /// sign-in keeps waiting (sicurezza-3).
    Foreign,
}

/// Reads a callback URL against the state this sign-in sent. The state is
/// checked first, so that neither a code nor an error from a request that
/// does not carry it can end the sign-in.
fn classify_callback(callback_url: &str, expected_state: &str) -> Result<CallbackOutcome> {
    let url = Url::parse(callback_url)
        .with_context(|| format!("invalid Microsoft callback URL '{callback_url}'"))?;
    let query_pairs = url.query_pairs().collect::<Vec<_>>();
    let value = |name: &str| {
        query_pairs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.to_string())
    };

    let Some(state) = value("state").filter(|state| state == expected_state) else {
        return Ok(CallbackOutcome::Foreign);
    };

    if let Some(error) = value("error") {
        let description = value("error_description").unwrap_or_default();
        let message = format!("Microsoft OAuth error: {} {}", error, description)
            .trim()
            .to_string();
        return Ok(CallbackOutcome::Failed(message));
    }

    Ok(match value("code") {
        Some(code) => CallbackOutcome::Code(AuthorizationCallback { code, state }),
        None => CallbackOutcome::Failed("callback is missing authorization code".to_string()),
    })
}

pub fn parse_authorization_callback(
    callback_url: &str,
    expected_state: &str,
) -> Result<AuthorizationCallback> {
    match classify_callback(callback_url, expected_state)? {
        CallbackOutcome::Code(callback) => Ok(callback),
        CallbackOutcome::Failed(message) => Err(anyhow!(message)),
        CallbackOutcome::Foreign => bail!("callback state mismatch"),
    }
}

pub fn build_pkce_code_challenge(code_verifier: &str) -> String {
    let digest = Sha256::digest(code_verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

pub fn generate_url_safe_random_bytes(byte_count: usize) -> String {
    let mut bytes = vec![0_u8; byte_count];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn validate_oauth_config(config: &MicrosoftOAuthConfig) -> Result<()> {
    if config.client_id.trim().is_empty() {
        bail!("client_id cannot be empty");
    }

    if config.redirect_uri.trim().is_empty() {
        bail!("redirect_uri cannot be empty");
    }

    if config.scopes.is_empty() {
        bail!("scopes cannot be empty");
    }

    Url::parse(&config.redirect_uri)
        .with_context(|| format!("invalid redirect_uri '{}'", config.redirect_uri))?;

    Ok(())
}

pub fn join_scopes(scopes: &[String]) -> String {
    scopes.join(" ")
}

pub fn default_loopback_redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}{LOOPBACK_REDIRECT_PATH}")
}

pub fn default_agent_author_name(connection: &Connection) -> Result<Option<String>> {
    AccountsRepository::new(connection)
        .load_active_account()
        .map(|account| account.and_then(|entry| entry.xbox_gamertag))
}

// ── Xbox Live / XSTS / Minecraft auth chain ─────────────────────────────────

const XBOX_LIVE_AUTH_URL: &str = "https://user.auth.xboxlive.com/user/authenticate";
const XSTS_AUTH_URL: &str = "https://xsts.auth.xboxlive.com/xsts/authorize";
const MINECRAFT_AUTH_URL: &str = "https://api.minecraftservices.com/authentication/login_with_xbox";
const MINECRAFT_PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct XboxLiveResponse {
    token: String,
    display_claims: XboxDisplayClaims,
}

#[derive(Debug, Clone, Deserialize)]
struct XboxDisplayClaims {
    xui: Vec<XboxUserInfo>,
}

#[derive(Debug, Clone, Deserialize)]
struct XboxUserInfo {
    uhs: String,
    #[serde(default)]
    gtg: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct MinecraftAuthResponse {
    access_token: String,
}

// ── The profile's skins and capes (E7) ──────────────────────────────────────
//
// `MinecraftProfile` is also the last step of the sign-in chain, so everything
// below is read defensively: a value Mojang adds later, or an element that is
// not what we expect, degrades to `Unknown` or is dropped. It never fails the
// profile, because failing the profile would fail every sign-in. Modrinth keeps
// the same `Unknown` fallback for variant and state.

/// Classic (4-pixel arms) or slim (3-pixel arms). Mojang writes `CLASSIC` and
/// `SLIM`; the launcher writes lowercase, which is also what the upload form
/// field takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkinVariant {
    #[serde(alias = "CLASSIC")]
    Classic,
    #[serde(alias = "SLIM")]
    Slim,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextureState {
    #[serde(alias = "ACTIVE")]
    Active,
    #[serde(alias = "INACTIVE")]
    Inactive,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileSkin {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub state: TextureState,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub texture_key: Option<String>,
    #[serde(default)]
    pub variant: SkinVariant,
    /// Set only on the skins Mojang assigns by default (`"KAI"` after a reset,
    /// measured in E7 phase A); absent on an uploaded skin.
    #[serde(default)]
    pub alias: Option<String>,
}

impl ProfileSkin {
    /// The texture's content address: the sha256 of the PNG Mojang serves.
    /// `textureKey` was present in every response measured; the URL's last
    /// segment is the same hash and covers a response without it.
    pub fn texture_key(&self) -> Option<&str> {
        self.texture_key
            .as_deref()
            .or_else(|| self.url.rsplit('/').next())
            .filter(|key| !key.is_empty())
    }
}

/// A cape the player **owns**. Capes cannot be added, only shown or hidden:
/// the list is every cape of the account, with at most one `Active`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileCape {
    pub id: String,
    #[serde(default)]
    pub state: TextureState,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MinecraftProfile {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "lenient_list")]
    pub skins: Vec<ProfileSkin>,
    #[serde(default, deserialize_with = "lenient_list")]
    pub capes: Vec<ProfileCape>,
}

impl MinecraftProfile {
    pub fn active_skin(&self) -> Option<&ProfileSkin> {
        self.skins
            .iter()
            .find(|skin| skin.state == TextureState::Active)
    }

    pub fn active_cape(&self) -> Option<&ProfileCape> {
        self.capes
            .iter()
            .find(|cape| cape.state == TextureState::Active)
    }
}

/// A list whose elements are read one by one: an element that does not parse
/// is dropped, and anything that is not an array (`null` included) is empty.
fn lenient_list<'de, D, T>(deserializer: D) -> std::result::Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| serde_json::from_value(item).ok())
            .collect(),
        _ => Vec::new(),
    })
}

/// Full auth chain: Microsoft access token → Xbox Live → XSTS → Minecraft token + profile.
pub struct MinecraftAuthChain {
    http_client: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct MinecraftLoginResult {
    pub microsoft_id: String,
    pub xbox_gamertag: Option<String>,
    pub minecraft_uuid: String,
    pub minecraft_username: String,
    pub minecraft_access_token: String,
    pub microsoft_refresh_token: Option<String>,
}

impl Default for MinecraftAuthChain {
    fn default() -> Self {
        Self::new()
    }
}

impl MinecraftAuthChain {
    pub fn new() -> Self {
        Self {
            http_client: auth_http_client(),
        }
    }

    pub async fn authenticate(
        &self,
        ms_access_token: &str,
        ms_refresh_token: Option<&str>,
        ms_user_id: Option<&str>,
    ) -> Result<MinecraftLoginResult> {
        // Step 1: Xbox Live
        let xbox = self.authenticate_xbox_live(ms_access_token).await?;
        let uhs = xbox
            .display_claims
            .xui
            .first()
            .context("Xbox Live response missing user hash")?;
        let gamertag = uhs.gtg.clone();
        let user_hash = uhs.uhs.clone();

        // Step 2: XSTS
        let xsts = self.authenticate_xsts(&xbox.token).await?;
        let xsts_token = xsts.token;

        // Step 3: Minecraft
        let mc_auth = self.authenticate_minecraft(&user_hash, &xsts_token).await?;

        // Step 4: Profile
        let profile = self.fetch_minecraft_profile(&mc_auth.access_token).await?;

        Ok(MinecraftLoginResult {
            microsoft_id: ms_user_id.unwrap_or(&profile.id).to_string(),
            xbox_gamertag: gamertag,
            minecraft_uuid: profile.id,
            minecraft_username: profile.name,
            minecraft_access_token: mc_auth.access_token,
            microsoft_refresh_token: ms_refresh_token.map(String::from),
        })
    }

    async fn authenticate_xbox_live(&self, ms_access_token: &str) -> Result<XboxLiveResponse> {
        let body = serde_json::json!({
            "Properties": {
                "AuthMethod": "RPS",
                "SiteName": "user.auth.xboxlive.com",
                "RpsTicket": format!("d={ms_access_token}")
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        });

        self.http_client
            .post(XBOX_LIVE_AUTH_URL)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to contact Xbox Live")?
            .error_for_status_with_body("Xbox Live authentication failed")
            .await?
            .json()
            .await
            .context("failed to parse Xbox Live response")
    }

    async fn authenticate_xsts(&self, xbox_token: &str) -> Result<XboxLiveResponse> {
        let body = serde_json::json!({
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [xbox_token]
            },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT"
        });

        self.http_client
            .post(XSTS_AUTH_URL)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to contact XSTS")?
            .error_for_status_with_body("XSTS authentication failed")
            .await?
            .json()
            .await
            .context("failed to parse XSTS response")
    }

    async fn authenticate_minecraft(
        &self,
        user_hash: &str,
        xsts_token: &str,
    ) -> Result<MinecraftAuthResponse> {
        let body = serde_json::json!({
            "identityToken": format!("XBL3.0 x={user_hash};{xsts_token}")
        });

        self.http_client
            .post(MINECRAFT_AUTH_URL)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .json(&body)
            .send()
            .await
            .context("failed to contact Minecraft services")?
            .error_for_status_with_body("Minecraft authentication failed")
            .await?
            .json()
            .await
            .context("failed to parse Minecraft auth response")
    }

    async fn fetch_minecraft_profile(&self, mc_access_token: &str) -> Result<MinecraftProfile> {
        self.http_client
            .get(MINECRAFT_PROFILE_URL)
            .header("Authorization", format!("Bearer {mc_access_token}"))
            .send()
            .await
            .context("failed to fetch Minecraft profile")?
            .error_for_status()
            .context("Minecraft profile request failed")?
            .json()
            .await
            .context("failed to parse Minecraft profile")
    }
}

trait ResponseErrorExt {
    async fn error_for_status_with_body(self, context: &str) -> Result<reqwest::Response>;
}

impl ResponseErrorExt for reqwest::Response {
    async fn error_for_status_with_body(self, context: &str) -> Result<reqwest::Response> {
        let status = self.status();
        if status.is_success() {
            return Ok(self);
        }

        let url = self.url().clone();
        let body = self.text().await.unwrap_or_default();
        let body = body.trim();

        if body.is_empty() {
            bail!("{context}: HTTP status {status} for url ({url})");
        }

        bail!("{context}: HTTP status {status} for url ({url}): {body}");
    }
}

// ── Login command with local callback server ────────────────────────────────

/// Loopback path registered in the Microsoft Entra app as `http://localhost/callback`.
pub const LOOPBACK_REDIRECT_PATH: &str = "/callback";
/// Loopback redirect URI registered without a port. Microsoft ignores the localhost
/// port during redirect URI matching for native applications.
pub const REGISTERED_LOOPBACK_REDIRECT_URI: &str = "http://localhost/callback";

/// Start Microsoft login using Authorization Code Flow with PKCE.
///
/// Opens the system browser, receives the Microsoft redirect on localhost,
/// validates state, exchanges the code, then continues through Xbox and
/// Minecraft services.
///
/// `on_authorization_url` receives the page the browser is sent to, before
/// the browser is asked to open it: the interface shows it, so that a
/// browser that opens in the wrong profile, or not at all, still leaves the
/// user a link to copy (account-3). A browser that can't be opened is
/// therefore not an error: the link is the way on.
pub async fn run_microsoft_login(
    client_id: &str,
    on_authorization_url: impl FnOnce(&str),
) -> Result<MinecraftLoginResult> {
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("localhost:0")
        .await
        .context("failed to bind local OAuth callback server")?;
    let port = listener.local_addr()?.port();

    let config = MicrosoftOAuthConfig {
        client_id: client_id.to_string(),
        redirect_uri: default_loopback_redirect_uri(port),
        scopes: vec!["XboxLive.signin".into(), "offline_access".into()],
    };

    let oauth_client = MicrosoftOAuthClient::new();
    let session = oauth_client.start_session(&config)?;

    on_authorization_url(session.authorization_url.as_str());
    if let Err(error) = open::that(session.authorization_url.as_str()) {
        eprintln!("[Login] Could not open the system browser, the link is shown instead: {error}");
    }

    let callback = serve_loopback_callback(&listener, port, &session.state).await?;
    let ms_tokens = oauth_client
        .exchange_authorization_code(&config, &callback, &session.code_verifier)
        .await?;

    // Full auth chain: Xbox Live → XSTS → Minecraft.
    let chain = MinecraftAuthChain::new();
    chain
        .authenticate(
            &ms_tokens.access_token,
            ms_tokens.refresh_token.as_deref(),
            ms_tokens.user_id.as_deref(),
        )
        .await
}

/// Waits for this sign-in's answer on the loopback listener. The page the
/// browser shows says what the launcher concluded: success only for a code
/// with this sign-in's state, an error page when Microsoft answered with an
/// error, and a 400 for a request that is not this sign-in's, after which
/// the wait goes on (sicurezza-3).
async fn serve_loopback_callback(
    listener: &tokio::net::TcpListener,
    port: u16,
    expected_state: &str,
) -> Result<AuthorizationCallback> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    loop {
        let (mut stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(300), listener.accept())
                .await
                .context("Microsoft login timed out (5 minutes)")?
                .context("failed to accept connection")?;

        let mut buf = vec![0_u8; 16384];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);

        let first_line = request.lines().next().unwrap_or("");

        let Some(callback_url) = callback_url_from_request_line(first_line, port) else {
            let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = stream.write_all(resp.as_bytes()).await;
            continue;
        };

        let outcome = classify_callback(&callback_url, expected_state)?;
        let _ = stream
            .write_all(callback_response(&outcome).as_bytes())
            .await;
        match outcome {
            CallbackOutcome::Code(callback) => return Ok(callback),
            CallbackOutcome::Failed(message) => return Err(anyhow!(message)),
            CallbackOutcome::Foreign => continue,
        }
    }
}

/// The whole HTTP response the browser gets for one callback request.
fn callback_response(outcome: &CallbackOutcome) -> String {
    let (status, body) = match outcome {
        CallbackOutcome::Code(_) => ("200 OK", success_callback_page().to_string()),
        CallbackOutcome::Failed(message) => ("200 OK", failed_callback_page(message)),
        CallbackOutcome::Foreign => ("400 Bad Request", foreign_callback_page().to_string()),
    };
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn callback_url_from_request_line(first_line: &str, port: u16) -> Option<String> {
    let mut parts = first_line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;

    let is_callback_target = target == LOOPBACK_REDIRECT_PATH
        || target
            .strip_prefix(LOOPBACK_REDIRECT_PATH)
            .is_some_and(|suffix| suffix.starts_with('?'));

    if method != "GET" || !is_callback_target {
        return None;
    }

    Some(format!("http://localhost:{port}{target}"))
}

fn callback_page(title: &str, text: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><title>Cubic Launcher</title></head>
<body style="font-family:system-ui;background:#101014;color:#f4f4f5;display:flex;align-items:center;justify-content:center;height:100vh;margin:0">
<main style="text-align:center;max-width:420px;padding:24px">
<h1 style="font-size:20px;margin:0 0 8px">{title}</h1>
<p style="color:#a1a1aa;margin:0">{text}</p>
</main>
</body></html>"#
    )
}

fn success_callback_page() -> String {
    callback_page(
        "Microsoft sign-in complete",
        "You can close this tab and return to Cubic Launcher.",
    )
}

fn failed_callback_page(message: &str) -> String {
    callback_page(
        "Microsoft sign-in did not complete",
        &format!(
            "{}<br><br>You can close this tab; Cubic Launcher shows the same error.",
            escape_html(message)
        ),
    )
}

fn foreign_callback_page() -> String {
    callback_page(
        "This is not the sign-in Cubic Launcher is waiting for",
        "The link belongs to another or an older sign-in. Cubic Launcher keeps waiting: finish the sign-in from the link it shows.",
    )
}

/// Microsoft's error text goes into the page as text, never as markup.
fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

pub fn microsoft_client_id_from_env(env_file_path: &Path) -> Result<Option<String>> {
    if !env_file_path.exists() {
        return Ok(None);
    }

    let contents = std::fs::read_to_string(env_file_path)
        .with_context(|| format!("failed to read {}", env_file_path.display()))?;

    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("MICROSOFT_CLIENT_ID=") {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(Some(trimmed.to_string()));
            }
        }
    }

    Ok(None)
}

/// The client id for Microsoft sign-in: the launcher's `.env`, then the
/// process environment, then the build environment, then the id bundled in
/// the launcher. There is always one, so the only error is an unreadable
/// `.env`.
pub fn configured_microsoft_client_id(env_file_path: &Path) -> Result<String> {
    Ok(microsoft_client_id_from_env(env_file_path)?
        .or_else(microsoft_client_id_from_runtime_env)
        .or_else(microsoft_client_id_from_build_env)
        .unwrap_or_else(|| BUNDLED_MICROSOFT_CLIENT_ID.to_string()))
}

fn microsoft_client_id_from_runtime_env() -> Option<String> {
    std::env::var("MICROSOFT_CLIENT_ID")
        .ok()
        .and_then(normalize_microsoft_client_id)
}

fn microsoft_client_id_from_build_env() -> Option<String> {
    option_env!("MICROSOFT_CLIENT_ID").and_then(normalize_microsoft_client_id)
}

fn normalize_microsoft_client_id(value: impl AsRef<str>) -> Option<String> {
    let trimmed = value.as_ref().trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use crate::database::initialize_database;

    use super::{
        build_authorization_url, build_pkce_code_challenge, callback_response,
        callback_url_from_request_line, classify_callback, default_agent_author_name,
        default_loopback_redirect_uri, generate_url_safe_random_bytes, join_scopes,
        microsoft_client_id_from_env, parse_authorization_callback, refresh_needs_sign_in_again,
        serve_loopback_callback, validate_oauth_config, AccountRecord, AccountsRepository,
        AuthorizationCallback, CallbackOutcome, MicrosoftOAuthClient, MicrosoftOAuthConfig,
        MinecraftProfile, SessionTokenUpdate, SkinVariant, TextureState,
    };

    fn unique_test_root() -> PathBuf {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();

        env::temp_dir().join(format!("cubic-launcher-microsoft-auth-test-{timestamp}"))
    }

    fn sample_config() -> MicrosoftOAuthConfig {
        MicrosoftOAuthConfig {
            client_id: "test-client-id".into(),
            redirect_uri: "http://127.0.0.1:43821/callback".into(),
            scopes: vec!["XboxLive.signin".into(), "offline_access".into()],
        }
    }

    #[test]
    fn pkce_code_challenge_matches_known_reference() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = build_pkce_code_challenge(verifier);

        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn start_session_builds_valid_authorization_url_and_pkce_pair() {
        let client = MicrosoftOAuthClient::new();
        let session = client
            .start_session(&sample_config())
            .expect("session should build");

        assert!(!session.state.is_empty());
        assert!(session.code_verifier.len() >= 43);
        assert_eq!(
            session.code_challenge,
            build_pkce_code_challenge(&session.code_verifier)
        );
        assert_eq!(
            session
                .authorization_url
                .query_pairs()
                .find(|(key, _)| key == "response_type")
                .unwrap()
                .1,
            "code"
        );
    }

    #[test]
    fn builds_authorization_url_with_expected_query_parameters() {
        let url = build_authorization_url(
            "https://login.microsoftonline.com/consumers/oauth2/v2.0/authorize",
            &sample_config(),
            "state123",
            "challenge456",
        )
        .expect("authorization url should build");

        let query_pairs = url.query_pairs().collect::<Vec<_>>();
        assert!(query_pairs
            .iter()
            .any(|(key, value)| key == "client_id" && value == "test-client-id"));
        assert!(query_pairs
            .iter()
            .any(|(key, value)| key == "state" && value == "state123"));
        assert!(query_pairs
            .iter()
            .any(|(key, value)| key == "code_challenge" && value == "challenge456"));
        assert!(query_pairs
            .iter()
            .any(|(key, value)| key == "scope" && value == "XboxLive.signin offline_access"));
    }

    #[test]
    fn parses_authorization_callback_and_validates_state() {
        let callback = parse_authorization_callback(
            "http://127.0.0.1:43821/callback?code=abc123&state=state123",
            "state123",
        )
        .expect("callback should parse");

        assert_eq!(callback.code, "abc123");
        assert_eq!(callback.state, "state123");
    }

    #[test]
    fn rejects_callback_state_mismatch() {
        let error = parse_authorization_callback(
            "http://127.0.0.1:43821/callback?code=abc123&state=wrong-state",
            "state123",
        )
        .expect_err("callback should fail");

        assert!(error.to_string().contains("callback state mismatch"));
    }

    #[test]
    fn validates_oauth_config() {
        validate_oauth_config(&sample_config()).expect("config should be valid");
        assert_eq!(
            join_scopes(&sample_config().scopes),
            "XboxLive.signin offline_access"
        );
        assert_eq!(
            default_loopback_redirect_uri(43821),
            "http://localhost:43821/callback"
        );
    }

    #[test]
    fn builds_callback_url_from_loopback_request_line() {
        let callback_url = callback_url_from_request_line(
            "GET /callback?code=abc123&state=state123 HTTP/1.1",
            43821,
        )
        .expect("callback url should be reconstructed");

        assert_eq!(
            callback_url,
            "http://localhost:43821/callback?code=abc123&state=state123"
        );
        assert!(callback_url_from_request_line("GET /favicon.ico HTTP/1.1", 43821).is_none());
        assert!(callback_url_from_request_line("GET /callbackevil HTTP/1.1", 43821).is_none());
        assert!(callback_url_from_request_line("POST /callback HTTP/1.1", 43821).is_none());
    }

    #[test]
    fn callback_without_this_sign_ins_state_is_foreign_even_with_a_code_or_an_error() {
        let base = "http://localhost:43821/callback";
        for url in [
            format!("{base}?code=abc&state=other"),
            format!("{base}?code=abc"),
            format!("{base}?error=access_denied&state=other"),
            format!("{base}?error=access_denied"),
        ] {
            assert_eq!(
                classify_callback(&url, "state123").expect("url should parse"),
                CallbackOutcome::Foreign,
                "{url}"
            );
        }
    }

    #[test]
    fn callback_error_with_this_sign_ins_state_fails_the_sign_in() {
        let outcome = classify_callback(
            "http://localhost:43821/callback?error=access_denied&error_description=The+user+said+no&state=state123",
            "state123",
        )
        .expect("url should parse");
        assert_eq!(
            outcome,
            CallbackOutcome::Failed("Microsoft OAuth error: access_denied The user said no".into())
        );
        assert_eq!(
            classify_callback("http://localhost:43821/callback?state=state123", "state123")
                .expect("url should parse"),
            CallbackOutcome::Failed("callback is missing authorization code".into())
        );
    }

    #[test]
    fn callback_page_says_what_the_launcher_concluded() {
        let success = callback_response(&CallbackOutcome::Code(AuthorizationCallback {
            code: "abc".into(),
            state: "state123".into(),
        }));
        assert!(success.starts_with("HTTP/1.1 200 OK"));
        assert!(success.contains("Microsoft sign-in complete"));

        let failed = callback_response(&CallbackOutcome::Failed(
            "Microsoft OAuth error: <script>".into(),
        ));
        assert!(failed.starts_with("HTTP/1.1 200 OK"));
        assert!(failed.contains("did not complete"));
        assert!(failed.contains("&lt;script&gt;"));
        assert!(!failed.contains("<script>"));
        assert!(!failed.contains("sign-in complete"));

        let foreign = callback_response(&CallbackOutcome::Foreign);
        assert!(foreign.starts_with("HTTP/1.1 400 Bad Request"));
        assert!(!foreign.contains("sign-in complete"));
    }

    #[tokio::test]
    async fn loopback_answers_400_to_a_foreign_callback_and_keeps_waiting_for_its_own() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener should bind");
        let port = listener
            .local_addr()
            .expect("listener has an address")
            .port();
        let server =
            tokio::spawn(async move { serve_loopback_callback(&listener, port, "state123").await });

        let send = |target: &'static str| async move {
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("loopback should accept");
            stream
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
                .await
                .expect("request should send");
            let mut response = String::new();
            stream
                .read_to_string(&mut response)
                .await
                .expect("response should read");
            response
        };

        let foreign = send("/callback?code=stolen&state=other").await;
        assert!(foreign.starts_with("HTTP/1.1 400"), "{foreign}");
        let own = send("/callback?code=abc123&state=state123").await;
        assert!(own.starts_with("HTTP/1.1 200"), "{own}");

        let callback = server
            .await
            .expect("server task should finish")
            .expect("the own callback should be accepted");
        assert_eq!(callback.code, "abc123");
    }

    #[test]
    fn deleting_the_active_account_activates_the_most_recent_other_one() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let repository = AccountsRepository::new(&connection);
        for (microsoft_id, last_login, is_active) in [
            ("account-old", "2026-01-01 10:00:00", false),
            ("account-recent", "2026-06-01 10:00:00", false),
            ("account-active", "2026-03-01 10:00:00", true),
        ] {
            repository
                .upsert_account(&AccountRecord {
                    microsoft_id: microsoft_id.into(),
                    xbox_gamertag: None,
                    minecraft_uuid: None,
                    access_token_enc: None,
                    refresh_token_enc: None,
                    profile_data: None,
                    is_active,
                    plays_offline: false,
                })
                .expect("account should insert");
            connection
                .execute(
                    "UPDATE accounts SET last_login = ?1 WHERE microsoft_id = ?2",
                    [last_login, microsoft_id],
                )
                .expect("last login should set");
        }

        // An inactive row goes without touching which one is active.
        repository
            .delete_account("account-old")
            .expect("inactive account should delete");
        assert_eq!(
            repository
                .load_active_account()
                .expect("active query should succeed")
                .map(|account| account.microsoft_id),
            Some("account-active".into())
        );

        repository
            .delete_account("account-active")
            .expect("active account should delete");
        assert_eq!(
            repository
                .load_active_account()
                .expect("active query should succeed")
                .map(|account| account.microsoft_id),
            Some("account-recent".into())
        );

        repository
            .delete_account("account-recent")
            .expect("last account should delete");
        assert!(repository
            .load_active_account()
            .expect("active query should succeed")
            .is_none());
        assert!(repository.delete_account("account-recent").is_err());

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn generated_random_value_is_url_safe() {
        let value = generate_url_safe_random_bytes(32);
        assert!(value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        }));
    }

    #[test]
    fn account_repository_upserts_lists_and_switches_active_account() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");

        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let repository = AccountsRepository::new(&connection);

        repository
            .upsert_account(&AccountRecord {
                microsoft_id: "account-a".into(),
                xbox_gamertag: Some("PlayerA".into()),
                minecraft_uuid: Some("uuid-a".into()),
                access_token_enc: Some(vec![1, 2, 3]),
                refresh_token_enc: Some(vec![4, 5, 6]),
                profile_data: Some("{\"name\":\"PlayerA\"}".into()),
                is_active: true,
                plays_offline: false,
            })
            .expect("first account should insert");
        repository
            .upsert_account(&AccountRecord {
                microsoft_id: "account-b".into(),
                xbox_gamertag: Some("PlayerB".into()),
                minecraft_uuid: Some("uuid-b".into()),
                access_token_enc: Some(vec![7, 8, 9]),
                refresh_token_enc: Some(vec![10, 11, 12]),
                profile_data: Some("{\"name\":\"PlayerB\"}".into()),
                is_active: false,
                plays_offline: false,
            })
            .expect("second account should insert");

        repository
            .set_active_account("account-b")
            .expect("active account should switch");

        let active_account = repository
            .load_active_account()
            .expect("active account query should succeed")
            .expect("active account should exist");
        let accounts = repository.list_accounts().expect("accounts should list");

        assert_eq!(accounts.len(), 2);
        assert_eq!(active_account.microsoft_id, "account-b");
        assert_eq!(active_account.xbox_gamertag.as_deref(), Some("PlayerB"));
        assert_eq!(
            default_agent_author_name(&connection).expect("author query should succeed"),
            Some("PlayerB".into())
        );

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    #[test]
    fn loads_client_id_from_env_style_file() {
        let root_dir = unique_test_root();
        let env_path = root_dir.join(".env");

        fs::create_dir_all(&root_dir).expect("root dir should exist");
        fs::write(
            &env_path,
            "MICROSOFT_CLIENT_ID=test-client-id\nOTHER_KEY=value\n",
        )
        .expect("env file should be written");

        let client_id = microsoft_client_id_from_env(&env_path)
            .expect("env file should parse")
            .expect("client id should exist");

        assert_eq!(client_id, "test-client-id");

        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// The shape `GET /minecraft/profile` returned for a real account on
    /// 2026-09-22 (E7 phase A), with the player's id, name and cape ids
    /// replaced. Texture keys are public and kept.
    const MEASURED_PROFILE: &str = r#"{
        "id": "0123456789abcdef0123456789abcdef",
        "name": "Player",
        "skins": [{
            "id": "6edef170-324a-46cd-a6de-584470b34b15",
            "state": "ACTIVE",
            "url": "http://textures.minecraft.net/texture/9df9e241bf5af8500d7146fcecdb36606d8b885ae41d0e08a4539ccf7221655c",
            "textureKey": "9df9e241bf5af8500d7146fcecdb36606d8b885ae41d0e08a4539ccf7221655c",
            "variant": "CLASSIC"
        }],
        "capes": [{
            "id": "00000000-0000-4000-8000-000000000001",
            "state": "INACTIVE",
            "url": "http://textures.minecraft.net/texture/28de4a81688ad18b49e735a273e086c18f1e3966956123ccb574034c06f5d336",
            "alias": "Pan"
        }, {
            "id": "00000000-0000-4000-8000-000000000002",
            "state": "ACTIVE",
            "url": "http://textures.minecraft.net/texture/2340c0e03dd24a11b15a8b33c2a7e9e32abb2051b2481d0ba7defd635ca7a933",
            "alias": "Migrator"
        }],
        "profileActions": {}
    }"#;

    #[test]
    fn profile_keeps_the_skins_and_capes_mojang_sends() {
        let profile: MinecraftProfile =
            serde_json::from_str(MEASURED_PROFILE).expect("the measured profile must parse");

        let skin = profile.active_skin().expect("the active skin must be kept");
        assert_eq!(
            skin.texture_key(),
            Some("9df9e241bf5af8500d7146fcecdb36606d8b885ae41d0e08a4539ccf7221655c")
        );
        assert_eq!(skin.variant, SkinVariant::Classic);
        assert_eq!(skin.state, TextureState::Active);

        assert_eq!(profile.capes.len(), 2, "every owned cape, active or not");
        assert_eq!(profile.capes[0].alias.as_deref(), Some("Pan"));
        assert_eq!(profile.capes[0].state, TextureState::Inactive);
        assert_eq!(
            profile.active_cape().map(|cape| cape.id.as_str()),
            Some("00000000-0000-4000-8000-000000000002")
        );
    }

    /// The profile is also the last step of the sign-in chain: a value Mojang
    /// adds tomorrow must not turn every sign-in into an error.
    #[test]
    fn profile_survives_values_mojang_may_add_tomorrow() {
        let profile: MinecraftProfile = serde_json::from_str(
            r#"{
                "id": "0123456789abcdef0123456789abcdef",
                "name": "Player",
                "skins": [
                    { "id": "a", "state": "EQUIPPED", "url": "http://textures.minecraft.net/texture/abc", "variant": "WIDE" },
                    42
                ],
                "capes": null
            }"#,
        )
        .expect("unknown values must not fail the profile");

        assert_eq!(
            profile.skins.len(),
            1,
            "an element that is not a skin is dropped, the rest kept"
        );
        assert_eq!(profile.skins[0].variant, SkinVariant::Unknown);
        assert_eq!(profile.skins[0].state, TextureState::Unknown);
        assert_eq!(
            profile.skins[0].texture_key(),
            Some("abc"),
            "without `textureKey` the key is the texture URL's last segment"
        );
        assert!(profile.capes.is_empty());
        assert!(
            profile.active_skin().is_none(),
            "an unknown state is not `ACTIVE`"
        );
    }

    #[test]
    fn profile_without_skins_or_capes_still_reads() {
        let profile: MinecraftProfile = serde_json::from_str(
            r#"{ "id": "0123456789abcdef0123456789abcdef", "name": "Player" }"#,
        )
        .expect("the two fields sign-in needs are enough");

        assert_eq!(profile.name, "Player");
        assert!(profile.skins.is_empty());
        assert!(profile.capes.is_empty());
    }

    #[test]
    fn skin_variant_round_trips_in_the_launcher_spelling() {
        assert_eq!(
            serde_json::to_string(&SkinVariant::Slim).expect("variant must serialize"),
            r#""slim""#
        );
        assert_eq!(
            serde_json::from_str::<SkinVariant>(r#""slim""#)
                .expect("our own spelling must read back"),
            SkinVariant::Slim
        );
    }

    /// A local token endpoint that answers one request with `status` and a
    /// JSON `body`.
    fn one_shot_token_endpoint(status: &'static str, body: &'static str) -> String {
        use std::io::{Read, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("test server should bind");
        let port = listener.local_addr().expect("server address").port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("one request should arrive");
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        format!("http://127.0.0.1:{port}/token")
    }

    async fn refresh_against(status: &'static str, body: &'static str) -> anyhow::Error {
        let token_url = one_shot_token_endpoint(status, body);
        MicrosoftOAuthClient::with_endpoints("http://127.0.0.1:9/authorize", token_url)
            .refresh_access_token(&sample_config(), "saved-refresh-token")
            .await
            .expect_err("the endpoint refuses the refresh")
    }

    /// D143 (account-1): a refresh token Microsoft refuses for good asks for a
    /// new sign-in; a refusal of the client or a server error does not, and
    /// the error never carries the token that was sent.
    #[tokio::test]
    async fn only_a_dead_grant_asks_to_sign_in_again() {
        let invalid_grant = refresh_against(
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"AADSTS70000: expired"}"#,
        )
        .await;
        assert!(refresh_needs_sign_in_again(&invalid_grant));
        assert!(format!("{invalid_grant:#}").contains("invalid_grant"));
        assert!(!format!("{invalid_grant:#}").contains("saved-refresh-token"));

        let interaction =
            refresh_against("400 Bad Request", r#"{"error":"interaction_required"}"#).await;
        assert!(refresh_needs_sign_in_again(&interaction));

        let client = refresh_against("400 Bad Request", r#"{"error":"invalid_client"}"#).await;
        assert!(!refresh_needs_sign_in_again(&client));

        let unavailable = refresh_against("503 Service Unavailable", "").await;
        assert!(!refresh_needs_sign_in_again(&unavailable));
    }

    fn signed_in_account(microsoft_id: &str, is_active: bool) -> AccountRecord {
        AccountRecord {
            microsoft_id: microsoft_id.into(),
            xbox_gamertag: Some(format!("{microsoft_id}-tag")),
            minecraft_uuid: Some(format!("{microsoft_id}-uuid")),
            access_token_enc: Some(vec![1, 2, 3]),
            refresh_token_enc: Some(vec![4, 5, 6]),
            profile_data: Some("{\"name\":\"Before\"}".into()),
            is_active,
            plays_offline: false,
        }
    }

    /// concorrenza-8: the launch's refresh writes the tokens of its own row and
    /// nothing else; `None` keeps what the row has.
    #[test]
    fn session_token_update_touches_only_the_tokens_of_its_row() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let repository = AccountsRepository::new(&connection);
        repository
            .upsert_account(&signed_in_account("account-a", true))
            .expect("A should insert");
        repository
            .set_plays_offline("account-a", true)
            .expect("A's choice should save");
        repository
            .upsert_account(&signed_in_account("account-b", true))
            .expect("B should insert and become active");

        let updated = repository
            .update_session_tokens(
                "account-a",
                &SessionTokenUpdate {
                    refresh_token_enc: vec![9, 9, 9],
                    access_token_enc: None,
                    minecraft_uuid: None,
                    profile_data: None,
                },
            )
            .expect("update should run");
        assert!(updated);
        let missing = repository
            .update_session_tokens(
                "account-gone",
                &SessionTokenUpdate {
                    refresh_token_enc: vec![9],
                    access_token_enc: None,
                    minecraft_uuid: None,
                    profile_data: None,
                },
            )
            .expect("update should run");
        assert!(!missing);

        let accounts = repository.list_accounts().expect("accounts should list");
        assert_eq!(accounts.len(), 2, "no row is created");
        let account_a = accounts
            .iter()
            .find(|account| account.microsoft_id == "account-a")
            .expect("A exists");
        assert_eq!(account_a.refresh_token_enc.as_deref(), Some(&[9, 9, 9][..]));
        assert_eq!(account_a.access_token_enc.as_deref(), Some(&[1, 2, 3][..]));
        assert_eq!(account_a.minecraft_uuid.as_deref(), Some("account-a-uuid"));
        assert!(!account_a.is_active);
        assert!(account_a.plays_offline);
        assert_eq!(
            repository
                .load_active_account()
                .expect("active should load")
                .expect("one is active")
                .microsoft_id,
            "account-b"
        );

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }

    /// D143: the refused refresh token is dropped only while the row still
    /// holds it; a sign-in saved during the launch is kept.
    #[test]
    fn a_rejected_refresh_token_is_cleared_only_if_it_is_still_the_saved_one() {
        let root_dir = unique_test_root();
        let database_path = root_dir.join("launcher_data.db");
        initialize_database(&database_path).expect("database should initialize");
        let connection = Connection::open(&database_path).expect("database should open");
        let repository = AccountsRepository::new(&connection);
        repository
            .upsert_account(&signed_in_account("account-a", true))
            .expect("A should insert");

        let stale = repository
            .clear_rejected_refresh_token("account-a", &[7, 7, 7])
            .expect("clear should run");
        assert!(!stale, "a newer sign-in is not thrown away");
        let cleared = repository
            .clear_rejected_refresh_token("account-a", &[4, 5, 6])
            .expect("clear should run");
        assert!(cleared);

        let account_a = repository
            .load_active_account()
            .expect("active should load")
            .expect("A stays active");
        assert_eq!(account_a.refresh_token_enc, None);
        assert_eq!(account_a.minecraft_uuid.as_deref(), Some("account-a-uuid"));

        drop(connection);
        fs::remove_dir_all(&root_dir).expect("temporary root should be removable");
    }
}
