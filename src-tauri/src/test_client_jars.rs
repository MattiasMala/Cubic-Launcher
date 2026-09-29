//! I `client.jar` veri che alcuni test leggono dalla cache del launcher
//! (`$HOME/.local/share/com.cubic.launcher/cache/minecraft/<versione>/client.jar`).
//!
//! Dove il jar manca, il test salta: una macchina senza la cache non ha niente
//! da provare. Ma un salto è un verde che non prova niente, e in CI, dove la
//! cache non esiste, lo sarebbe a ogni run. Con [`REQUIRE_ENV`] impostata il
//! salto diventa un fallimento che nomina il file che manca.
//!
//! La regola sta solo qui: [`cached_client_jar`] per trovare il jar,
//! [`unavailable`] per ogni altro motivo per cui un test non può usarlo.

use std::fmt::Display;
use std::path::PathBuf;

/// Impostata e non vuota (come `CUBIC_AUTOMATION_VERIFY_REQUEST`), un jar che
/// manca fa fallire il test invece di farlo saltare.
pub(crate) const REQUIRE_ENV: &str = "CUBIC_REQUIRE_CLIENT_JARS";

fn required() -> bool {
    std::env::var(REQUIRE_ENV)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// Il test non può usare il jar. Senza [`REQUIRE_ENV`] è `None`, e il
/// chiamante salta come ha sempre fatto; con [`REQUIRE_ENV`] è un panic con
/// il motivo.
pub(crate) fn unavailable<T>(reason: impl Display) -> Option<T> {
    if required() {
        panic!("{REQUIRE_ENV} is set, so this test may not skip: {reason}");
    }
    None
}

/// Il `client.jar` di `version` nella cache del launcher, se c'è.
pub(crate) fn cached_client_jar(version: &str) -> Option<PathBuf> {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return unavailable(format!(
            "HOME is not set, so the client.jar of {version} cannot be located"
        ));
    };
    let jar = home
        .join(".local/share/com.cubic.launcher/cache/minecraft")
        .join(version)
        .join("client.jar");
    if jar.is_file() {
        Some(jar)
    } else {
        unavailable(format!("{} is missing", jar.display()))
    }
}
