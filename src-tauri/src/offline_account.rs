use anyhow::{Context, Result};
use serde_json::Value;
use uuid::Uuid;

pub fn deterministic_offline_uuid(username: &str) -> Uuid {
    Uuid::new_v5(&offline_uuid_namespace(), username.as_bytes())
}

pub fn resolve_cached_profile_username(
    profile_data: Option<&str>,
    xbox_gamertag: Option<&str>,
) -> Result<Option<String>> {
    if let Some(profile_data) = profile_data {
        let parsed = serde_json::from_str::<Value>(profile_data)
            .with_context(|| "failed to parse cached account profile_data JSON".to_string())?;

        if let Some(username) = find_username_in_profile_json(&parsed) {
            return Ok(Some(username));
        }
    }

    Ok(xbox_gamertag
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string))
}

fn find_username_in_profile_json(value: &Value) -> Option<String> {
    const USERNAME_KEYS: &[&str] = &["username", "name", "preferred_username", "gamertag"];

    match value {
        Value::Object(map) => {
            for key in USERNAME_KEYS {
                if let Some(username) = map
                    .get(*key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                {
                    return Some(username.to_string());
                }
            }

            map.values().find_map(find_username_in_profile_json)
        }
        Value::Array(values) => values.iter().find_map(find_username_in_profile_json),
        _ => None,
    }
}

fn offline_uuid_namespace() -> Uuid {
    Uuid::from_u128(0x9dc8a124_4f68_4b12_9af6_5e7f2c9c1a01)
}

#[cfg(test)]
mod tests {
    use super::{deterministic_offline_uuid, resolve_cached_profile_username};

    #[test]
    fn deterministic_offline_uuid_is_stable_for_same_username() {
        let first = deterministic_offline_uuid("PlayerOne");
        let second = deterministic_offline_uuid("PlayerOne");
        let third = deterministic_offline_uuid("PlayerTwo");

        assert_eq!(first, second);
        assert_ne!(first, third);
    }

    #[test]
    fn resolves_username_from_cached_profile_json_before_gamertag() {
        let username = resolve_cached_profile_username(
            Some(r#"{"profile":{"name":"CachedPlayer"}}"#),
            Some("GamertagFallback"),
        )
        .expect("username should resolve");

        assert_eq!(username.as_deref(), Some("CachedPlayer"));
    }

    #[test]
    fn falls_back_to_gamertag_when_profile_json_has_no_username() {
        let username = resolve_cached_profile_username(
            Some(r#"{"profile":{"id":"abc"}}"#),
            Some("GamertagFallback"),
        )
        .expect("username should resolve");

        assert_eq!(username.as_deref(), Some("GamertagFallback"));
    }
}
