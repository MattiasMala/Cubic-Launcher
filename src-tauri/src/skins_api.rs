//! Mojang's side of E7: the calls, how their errors read, the guard on the
//! write rate, and the sign-in every call rides on.
//!
//! What was **measured** on a real account (E7 phase 1, report 061) and what
//! is only **assumed** is kept apart in the comments below: the shape of each
//! answer is measured, the write rate is not — measuring it means producing
//! 429s on purpose, and a high volume of 429s on skin uploads is a documented
//! cause of account suspension.

// A build with the fake backend (`skins_fake.rs`) never reaches the live one.
#![cfg_attr(feature = "skins-fake-backend", allow(dead_code))]

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rusqlite::Connection;
use serde::Serialize;

use crate::launcher_paths::LauncherPaths;
use crate::microsoft_auth::{
    configured_microsoft_client_id, AccountsRepository, MicrosoftOAuthClient, MicrosoftOAuthConfig,
    MinecraftAuthChain, MinecraftProfile, SkinVariant, REGISTERED_LOOPBACK_REDIRECT_URI,
};
use crate::token_storage::{
    AccountTokenCipher, EncryptedAccountsRepository, KeyringSecretStore, PlaintextAccountRecord,
};

use super::{SkinsState, MAX_SKIN_BYTES};

const PROFILE_URL: &str = "https://api.minecraftservices.com/minecraft/profile";
const TEXTURES_URL: &str = "https://textures.minecraft.net/texture";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Writes (skin upload, reset, cape show/hide) per minute that the launcher
/// allows itself. **Assumed, not measured**: 20/min is wiki.vg's figure for
/// `POST /minecraft/profile/skins`, a secondary source. The guard exists so
/// that the launcher never produces the burst of 429s Mojang's documentation
/// names as a suspension cause.
pub const ASSUMED_WRITES_PER_MINUTE: usize = 20;

/// How long to stay quiet after a 429 that carries no `Retry-After`.
pub const DEFAULT_RATE_LIMIT_COOLDOWN_SECONDS: u64 = 60;

const RATE_WINDOW: Duration = Duration::from_secs(60);

// ── Errors ─────────────────────────────────────────────────────────────────

/// What the skin screen needs to decide what to show. It reaches the
/// frontend as an object, so the interface reads `kind` and never has to
/// match on message text (Modrinth recognizes a 429 by searching the message
/// for "429 Too Many Requests").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkinErrorKind {
    /// No Microsoft account, a sign-in that can't be read or refreshed, or a
    /// 401 that a refresh did not cure: only signing in again helps.
    NotSignedIn,
    /// Mojang answered 429, or the local guard refused before asking.
    RateLimited,
    /// The texture is not a skin: refused locally or by Mojang.
    InvalidSkin,
    /// Mojang or Microsoft could not be reached.
    Network,
    /// Any other answer from Mojang.
    Mojang,
    /// The launcher's own side: database, files.
    Library,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkinError {
    pub kind: SkinErrorKind,
    pub message: String,
    /// The HTTP status, when the error is an answer.
    pub status: Option<u16>,
    /// For `RateLimited`: how long before a write is allowed again.
    pub retry_after_seconds: Option<u64>,
}

impl SkinError {
    pub fn new(kind: SkinErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            status: None,
            retry_after_seconds: None,
        }
    }

    pub(crate) fn library(error: impl fmt::Display) -> Self {
        Self::new(SkinErrorKind::Library, format!("{error:#}"))
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::new(SkinErrorKind::InvalidSkin, message)
    }

    fn not_signed_in(message: impl Into<String>) -> Self {
        Self::new(SkinErrorKind::NotSignedIn, message)
    }

    fn network(error: impl fmt::Display) -> Self {
        Self::new(
            SkinErrorKind::Network,
            format!("Couldn't reach Mojang: {error}"),
        )
    }
}

impl fmt::Display for SkinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Reads an error answer by its **status**. The body only supplies the
/// message, and for a 400 the one detail measured in phase A:
/// `{"details":{"status":"INVALID_IMAGE_DATA"},"errorMessage":"Invalid image data"}`
/// for a PNG Mojang can't decode.
pub fn classify_mojang_error(status: u16, retry_after: Option<&str>, body: &str) -> SkinError {
    let answer = serde_json::from_str::<serde_json::Value>(body).ok();
    let text = |pointer: &str| {
        answer
            .as_ref()
            .and_then(|answer| answer.pointer(pointer))
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let detail = text("/details/status");
    let message = text("/errorMessage").or_else(|| text("/error"));

    let mut error = match status {
        429 => {
            let seconds = retry_after
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN_SECONDS);
            let mut error = SkinError::new(
                SkinErrorKind::RateLimited,
                format!("Mojang asks to slow down; try again in {seconds} seconds"),
            );
            error.retry_after_seconds = Some(seconds);
            error
        }
        401 => SkinError::not_signed_in("Minecraft refused the saved sign-in"),
        400 if detail.as_deref() == Some("INVALID_IMAGE_DATA") => SkinError::invalid(
            message.unwrap_or_else(|| "Mojang could not read the image".to_string()),
        ),
        _ => SkinError::new(
            SkinErrorKind::Mojang,
            message.unwrap_or_else(|| format!("Mojang answered HTTP {status}")),
        ),
    };
    error.status = Some(status);
    error
}

// ── The write guard ────────────────────────────────────────────────────────

/// Counts the writes of the last minute and remembers a 429's `Retry-After`.
/// It refuses **before** a request leaves, so that a user clicking through
/// skins can't turn into a stream of 429s on their account.
#[derive(Debug, Default)]
pub struct WriteGuard {
    recent: VecDeque<Instant>,
    quiet_until: Option<Instant>,
}

impl WriteGuard {
    /// Lets one write through and counts it, or refuses it.
    pub fn admit(&mut self, now: Instant) -> Result<(), SkinError> {
        if let Some(until) = self.quiet_until {
            if now < until {
                return Err(refused_until(until - now));
            }
            self.quiet_until = None;
        }
        while self
            .recent
            .front()
            .is_some_and(|sent| now.saturating_duration_since(*sent) >= RATE_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= ASSUMED_WRITES_PER_MINUTE {
            let oldest = self.recent.front().copied().unwrap_or(now);
            return Err(refused_until(
                RATE_WINDOW.saturating_sub(now.saturating_duration_since(oldest)),
            ));
        }
        self.recent.push_back(now);
        Ok(())
    }

    /// Mojang answered 429: nothing more until `retry_after_seconds` passed.
    pub fn rate_limited(&mut self, now: Instant, retry_after_seconds: u64) {
        self.quiet_until = Some(now + Duration::from_secs(retry_after_seconds));
    }
}

fn refused_until(wait: Duration) -> SkinError {
    let seconds = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    let mut error = SkinError::new(
        SkinErrorKind::RateLimited,
        format!("Too many skin changes in a minute; try again in {seconds} seconds"),
    );
    error.retry_after_seconds = Some(seconds.max(1));
    error
}

// ── The backend the operations talk to ─────────────────────────────────────

/// Mojang as the skin operations see it. The live one signs the requests and
/// guards the writes; the tests' one answers like the Mojang measured in
/// phase A. Every write answers with the updated profile (measured for all
/// four).
pub(crate) trait SkinBackend {
    async fn profile(&self) -> Result<MinecraftProfile, SkinError>;
    async fn upload_skin(
        &self,
        png: Vec<u8>,
        variant: SkinVariant,
    ) -> Result<MinecraftProfile, SkinError>;
    async fn reset_skin(&self) -> Result<MinecraftProfile, SkinError>;
    /// `Some` shows that cape, `None` hides whichever is shown.
    async fn set_cape(&self, cape_id: Option<&str>) -> Result<MinecraftProfile, SkinError>;
    /// The public PNG Mojang serves under a texture key.
    async fn download_texture(&self, texture_key: &str) -> Result<Vec<u8>, SkinError>;
    /// The last profile read, for the operations that don't ask Mojang.
    fn cached_profile(&self) -> Option<MinecraftProfile>;
    fn remember_profile(&self, profile: &MinecraftProfile);
}

/// The URL the frontend loads a Mojang texture from. Mojang writes `http://`;
/// the same file answers on `https://`, with `Access-Control-Allow-Origin: *`
/// (both measured), which the 3D preview needs to use it as a WebGL texture.
pub fn texture_url(texture_key: &str) -> String {
    format!("{TEXTURES_URL}/{texture_key}")
}

// ── The live backend ───────────────────────────────────────────────────────

/// A Minecraft access token. `Debug` prints nothing of it: the token must
/// never reach a log, a report or an error message.
#[derive(Clone)]
struct AccessToken(String);

impl fmt::Debug for AccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccessToken(..)")
    }
}

pub(crate) struct LiveBackend<'a> {
    paths: &'a LauncherPaths,
    state: &'a SkinsState,
    http: reqwest::Client,
    token: Mutex<Option<AccessToken>>,
}

impl<'a> LiveBackend<'a> {
    pub(crate) fn new(paths: &'a LauncherPaths, state: &'a SkinsState) -> Result<Self, SkinError> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(SkinError::library)?;
        Ok(Self {
            paths,
            state,
            http,
            token: Mutex::new(None),
        })
    }

    /// Sends one Mojang request with the saved sign-in. A 401 means the
    /// Minecraft token expired (it lives 24 hours): the sign-in is refreshed
    /// the way a launch refreshes it, and the request is sent once more.
    async fn signed<T, F, Fut>(&self, is_write: bool, send: F) -> Result<T, SkinError>
    where
        F: Fn(reqwest::Client, AccessToken) -> Fut,
        Fut: Future<Output = Result<T, SkinError>>,
    {
        let mut token = self.token().await?;
        for attempt in 0..2 {
            if is_write {
                self.state.guard.lock().admit(Instant::now())?;
            }
            let result = send(self.http.clone(), token.clone()).await;
            match result {
                Err(error) if error.status == Some(401) && attempt == 0 => {
                    token = self.refresh().await?;
                }
                Err(error) => {
                    if error.status == Some(429) {
                        self.state.guard.lock().rate_limited(
                            Instant::now(),
                            error
                                .retry_after_seconds
                                .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN_SECONDS),
                        );
                    }
                    return Err(error);
                }
                Ok(value) => return Ok(value),
            }
        }
        Err(SkinError::not_signed_in(
            "Minecraft refused the sign-in even after refreshing it; sign in again from Manage Accounts",
        ))
    }

    async fn token(&self) -> Result<AccessToken, SkinError> {
        let cached = self.token.lock().clone();
        if let Some(token) = cached {
            return Ok(token);
        }
        match stored_access_token(self.paths.database_path())? {
            Some(token) => {
                *self.token.lock() = Some(token.clone());
                Ok(token)
            }
            None => self.refresh().await,
        }
    }

    async fn refresh(&self) -> Result<AccessToken, SkinError> {
        let token = refresh_minecraft_token(self.paths).await?;
        *self.token.lock() = Some(token.clone());
        Ok(token)
    }
}

impl SkinBackend for LiveBackend<'_> {
    async fn profile(&self) -> Result<MinecraftProfile, SkinError> {
        self.signed(false, |http, token| async move {
            read_profile(http.get(PROFILE_URL).bearer_auth(&token.0)).await
        })
        .await
    }

    async fn upload_skin(
        &self,
        png: Vec<u8>,
        variant: SkinVariant,
    ) -> Result<MinecraftProfile, SkinError> {
        let variant = match variant {
            SkinVariant::Classic => "classic",
            SkinVariant::Slim => "slim",
            SkinVariant::Unknown => {
                return Err(SkinError::invalid("A skin is uploaded as classic or slim"))
            }
        };
        self.signed(true, |http, token| {
            let png = png.clone();
            async move {
                let file = reqwest::multipart::Part::bytes(png)
                    .file_name("skin.png")
                    .mime_str("image/png")
                    .map_err(SkinError::library)?;
                let form = reqwest::multipart::Form::new()
                    .text("variant", variant)
                    .part("file", file);
                read_profile(
                    http.post(format!("{PROFILE_URL}/skins"))
                        .bearer_auth(&token.0)
                        .multipart(form),
                )
                .await
            }
        })
        .await
    }

    async fn reset_skin(&self) -> Result<MinecraftProfile, SkinError> {
        self.signed(true, |http, token| async move {
            read_profile(
                http.delete(format!("{PROFILE_URL}/skins/active"))
                    .bearer_auth(&token.0),
            )
            .await
        })
        .await
    }

    async fn set_cape(&self, cape_id: Option<&str>) -> Result<MinecraftProfile, SkinError> {
        self.signed(true, |http, token| async move {
            let url = format!("{PROFILE_URL}/capes/active");
            let request = match cape_id {
                Some(cape_id) => http
                    .put(url)
                    .json(&serde_json::json!({ "capeId": cape_id })),
                None => http.delete(url),
            };
            read_profile(request.bearer_auth(&token.0)).await
        })
        .await
    }

    async fn download_texture(&self, texture_key: &str) -> Result<Vec<u8>, SkinError> {
        let response = self
            .http
            .get(texture_url(texture_key))
            .send()
            .await
            .map_err(SkinError::network)?;
        let status = response.status();
        if !status.is_success() {
            let mut error = SkinError::new(
                SkinErrorKind::Mojang,
                format!("The texture server answered {status} for {texture_key}"),
            );
            error.status = Some(status.as_u16());
            return Err(error);
        }
        if response
            .content_length()
            .is_some_and(|length| length > MAX_SKIN_BYTES)
        {
            return Err(SkinError::invalid("The texture is larger than any skin"));
        }
        let bytes = response.bytes().await.map_err(SkinError::network)?;
        Ok(bytes.to_vec())
    }

    fn cached_profile(&self) -> Option<MinecraftProfile> {
        self.state.last_profile.lock().clone()
    }

    fn remember_profile(&self, profile: &MinecraftProfile) {
        *self.state.last_profile.lock() = Some(profile.clone());
    }
}

/// Sends a request that answers with the profile, and reads either the
/// profile or the error.
async fn read_profile(request: reqwest::RequestBuilder) -> Result<MinecraftProfile, SkinError> {
    let response = request.send().await.map_err(SkinError::network)?;
    let status = response.status();
    if status.is_success() {
        return response.json::<MinecraftProfile>().await.map_err(|error| {
            SkinError::new(
                SkinErrorKind::Mojang,
                format!("Mojang's answer is not a profile: {error}"),
            )
        });
    }
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.unwrap_or_default();
    Err(classify_mojang_error(
        status.as_u16(),
        retry_after.as_deref(),
        &body,
    ))
}

// ── The sign-in ────────────────────────────────────────────────────────────

fn open_database(db_path: &Path) -> Result<Connection, SkinError> {
    Connection::open(db_path).map_err(SkinError::library)
}

/// The active account's saved Minecraft token, or `None` when there is none
/// that decrypts (the refresh decides then).
fn stored_access_token(db_path: &Path) -> Result<Option<AccessToken>, SkinError> {
    let connection = open_database(db_path)?;
    let account = AccountsRepository::new(&connection)
        .load_active_account()
        .map_err(SkinError::library)?
        .ok_or_else(|| SkinError::not_signed_in("No Microsoft account is signed in"))?;
    Ok(account
        .access_token_enc
        .as_deref()
        .filter(|payload| !payload.is_empty())
        .and_then(|payload| {
            AccountTokenCipher::new(KeyringSecretStore::new())
                .decrypt_token(payload)
                .ok()
        })
        .map(AccessToken))
}

/// Refreshes the active account's sign-in with its Microsoft refresh token,
/// through the same chain and the same persistence order as a launch
/// (`launch_preview_runtime::load_player_identity`): the rotated refresh token
/// is saved before the Xbox/Minecraft chain, so a failure there can't lose it.
async fn refresh_minecraft_token(paths: &LauncherPaths) -> Result<AccessToken, SkinError> {
    let db_path = paths.database_path().to_path_buf();
    let (account, refresh_token) = {
        let connection = open_database(&db_path)?;
        let account = AccountsRepository::new(&connection)
            .load_active_account()
            .map_err(SkinError::library)?
            .ok_or_else(|| SkinError::not_signed_in("No Microsoft account is signed in"))?;
        let refresh_token = account
            .refresh_token_enc
            .as_deref()
            .filter(|payload| !payload.is_empty())
            .ok_or_else(|| SkinError::not_signed_in("The account has no saved sign-in; sign in again from Manage Accounts"))
            .and_then(|payload| {
                AccountTokenCipher::new(KeyringSecretStore::new())
                    .decrypt_token(payload)
                    .map_err(|_| {
                        SkinError::not_signed_in(
                            "The saved sign-in can't be read; use \"Sign in again\" in Manage Accounts",
                        )
                    })
            })?;
        (account, refresh_token)
    };

    let client_id = configured_microsoft_client_id(&paths.root_dir().join(".env"))
        .map_err(SkinError::library)?
        .ok_or_else(|| {
            SkinError::not_signed_in("Microsoft sign-in is not configured in this build")
        })?;
    let config = MicrosoftOAuthConfig {
        client_id,
        redirect_uri: REGISTERED_LOOPBACK_REDIRECT_URI.to_string(),
        scopes: vec!["XboxLive.signin".into(), "offline_access".into()],
    };

    let microsoft = MicrosoftOAuthClient::new()
        .refresh_access_token(&config, &refresh_token)
        .await
        .map_err(sign_in_failure)?;
    let rotated = microsoft
        .refresh_token
        .clone()
        .unwrap_or_else(|| refresh_token.clone());

    let mut record = PlaintextAccountRecord {
        microsoft_id: account.microsoft_id.clone(),
        xbox_gamertag: account.xbox_gamertag.clone(),
        minecraft_uuid: account.minecraft_uuid.clone(),
        access_token: None,
        refresh_token: Some(rotated),
        profile_data: account.profile_data.clone(),
        is_active: true,
    };
    save_account(&db_path, &record)?;

    let login = MinecraftAuthChain::new()
        .authenticate(
            &microsoft.access_token,
            microsoft.refresh_token.as_deref(),
            microsoft.user_id.as_deref(),
        )
        .await
        .map_err(sign_in_failure)?;

    record.minecraft_uuid = Some(login.minecraft_uuid.clone());
    record.access_token = Some(login.minecraft_access_token.clone());
    record.profile_data = Some(crate::app_shell::account_profile_data_json(
        &login.minecraft_username,
        &login.minecraft_uuid,
    ));
    save_account(&db_path, &record)?;

    Ok(AccessToken(login.minecraft_access_token))
}

fn save_account(db_path: &Path, record: &PlaintextAccountRecord) -> Result<(), SkinError> {
    let connection = open_database(db_path)?;
    EncryptedAccountsRepository::new(&connection, KeyringSecretStore::new())
        .upsert_account(record)
        .map_err(SkinError::library)
}

/// A refresh or chain failure: unreachable network, or a sign-in Microsoft
/// no longer accepts.
fn sign_in_failure(error: anyhow::Error) -> SkinError {
    let unreachable = error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_connect() || error.is_timeout())
    });
    if unreachable {
        SkinError::network(format!("{error:#}"))
    } else {
        SkinError::not_signed_in(format!(
            "The saved sign-in could not be refreshed ({error:#}); sign in again from Manage Accounts"
        ))
    }
}
