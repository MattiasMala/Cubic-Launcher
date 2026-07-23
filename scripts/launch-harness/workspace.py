"""Isolated, repeatable workspace for the Cubic Launcher launch-verification harness.

The launcher roots everything under `app_local_data_dir()`, which on Linux is
`$XDG_DATA_HOME/com.cubic.launcher` (or `$HOME/.local/share/com.cubic.launcher`
when XDG_DATA_HOME is unset). We therefore isolate a run by pointing
`XDG_DATA_HOME` at a fresh temp directory. The launcher NEVER sees the user's
real data dir.

Safety: every filesystem write asserts the target is under the temp root.

stdlib-only.
"""

from __future__ import annotations

import json
import shutil
import sqlite3
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

# Tauri identifier from src-tauri/tauri.conf.json ("com.cubic.launcher").
APP_IDENTIFIER = "com.cubic.launcher"
DATABASE_FILENAME = "launcher_data.db"

# EXACT accounts DDL from src-tauri/src/database.rs (CREATE TABLE IF NOT EXISTS).
# We pre-create + seed the accounts table; initialize_database() at startup
# creates every OTHER table via IF NOT EXISTS and leaves this one untouched.
ACCOUNTS_DDL = """
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
"""

# rules.json v4 schema (src-tauri/src/rules.rs). schema_version MUST be 4.
RULES_SCHEMA_VERSION = 4


@dataclass
class ModRule:
    """One rule in a modlist's rules.json. mod_id must be unique in the tree."""

    mod_id: str
    source: str = "modrinth"  # "modrinth" | "local"
    enabled: bool = True
    exclude_if: list[str] = field(default_factory=list)
    requires: list[str] = field(default_factory=list)
    version_rules: list[dict] = field(default_factory=list)
    custom_configs: list[dict] = field(default_factory=list)
    alternatives: list["ModRule"] = field(default_factory=list)

    def to_json(self) -> dict:
        return {
            "mod_id": self.mod_id,
            "source": self.source,
            "enabled": self.enabled,
            "exclude_if": self.exclude_if,
            "requires": self.requires,
            "version_rules": self.version_rules,
            "custom_configs": self.custom_configs,
            "alternatives": [rule.to_json() for rule in self.alternatives],
        }


@dataclass
class Modlist:
    name: str
    author: str = "Harness"
    description: str = "Launch-verification harness fixture"
    rules: list[ModRule] = field(default_factory=list)

    def to_json(self) -> dict:
        return {
            "schema_version": RULES_SCHEMA_VERSION,
            "modlist_name": self.name,
            "author": self.author,
            "description": self.description,
            "rules": [rule.to_json() for rule in self.rules],
        }


class HarnessWorkspace:
    """A disposable launcher data root under a temp directory.

    Usage:
        with HarnessWorkspace() as ws:
            ws.seed_offline_account()
            ws.create_modlist(Modlist("harness-vanilla"))
            env = ws.launcher_env()   # inject into the binary's environment
    """

    def __init__(self, keep: bool = False, base_dir: Path | None = None):
        self._keep = keep
        # mkdtemp always lives under the system temp dir (or base_dir) — never $HOME.
        self._tmp_root = Path(
            tempfile.mkdtemp(prefix="cubic-harness-", dir=base_dir)
        ).resolve()
        # XDG_DATA_HOME = tmp_root ; launcher root = tmp_root/com.cubic.launcher
        self.xdg_data_home = self._tmp_root
        self.launcher_root = self._tmp_root / APP_IDENTIFIER
        self._assert_under_tmp(self.launcher_root)
        self.launcher_root.mkdir(parents=True, exist_ok=True)

    # ---- safety ---------------------------------------------------------------

    def _assert_under_tmp(self, path: Path) -> None:
        resolved = Path(path).resolve()
        if self._tmp_root not in resolved.parents and resolved != self._tmp_root:
            raise RuntimeError(
                f"REFUSING to write outside the temp root: {resolved} not under {self._tmp_root}"
            )

    # ---- paths ----------------------------------------------------------------

    @property
    def database_path(self) -> Path:
        return self.launcher_root / DATABASE_FILENAME

    @property
    def modlists_dir(self) -> Path:
        return self.launcher_root / "mod-lists"

    @property
    def cache_dir(self) -> Path:
        return self.launcher_root / "cache"

    @property
    def launch_logs_dir(self) -> Path:
        return self.launcher_root / "logs" / "launches"

    # ---- seeding --------------------------------------------------------------

    def seed_offline_account(
        self,
        username: str = "HarnessPlayer",
        microsoft_id: str = "harness-account",
    ) -> None:
        """Pre-seed a single active offline account.

        Token columns are NULL and profile_data carries ONLY username/uuid — no
        mc_access_token / ms_refresh_token — so the startup token migration
        (KeyringSecretStore) never touches the real keyring.
        """
        self._assert_under_tmp(self.database_path)
        # Deterministic offline-style uuid string for display; the launcher
        # recomputes its own offline uuid when needed.
        profile_data = json.dumps({"username": username, "uuid": _offline_uuid(username)})
        connection = sqlite3.connect(self.database_path)
        try:
            connection.executescript(ACCOUNTS_DDL)
            connection.execute(
                """
                INSERT INTO accounts
                    (microsoft_id, xbox_gamertag, minecraft_uuid,
                     access_token_enc, refresh_token_enc, last_login,
                     profile_data, is_active)
                VALUES (?, ?, NULL, NULL, NULL, NULL, ?, 1)
                """,
                (microsoft_id, username, profile_data),
            )
            connection.commit()
        finally:
            connection.close()

    def reset_database(self) -> None:
        """Delete only launcher_data.db, preserving cache/ and mod-lists/.

        Used between the API pass and the cache pass, and before each launch, so
        the schema+token migration re-runs on a clean, token-free DB while the
        API-populated cache is reused.
        """
        if self.database_path.exists():
            self._assert_under_tmp(self.database_path)
            self.database_path.unlink()

    def create_modlist(self, modlist: Modlist) -> Path:
        target = self.modlists_dir / modlist.name
        self._assert_under_tmp(target)
        (target / "local-jars").mkdir(parents=True, exist_ok=True)
        (target / "custom_configs").mkdir(parents=True, exist_ok=True)
        rules_path = target / "rules.json"
        rules_path.write_text(json.dumps(modlist.to_json(), indent=2) + "\n")
        return target

    def set_modlist_cache_only(self, modlist_name: str, cache_only: bool) -> None:
        """Set the per-modlist "prioritize downloaded jars" (cache_only) override.

        Written to the modlist_settings table the launcher reads via
        load_shell_snapshot_from_root. Used to flip the whole matrix into the
        cache pass without changing global settings.
        """
        self._assert_under_tmp(self.database_path)
        connection = sqlite3.connect(self.database_path)
        try:
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS modlist_settings (
                    modlist_name TEXT NOT NULL,
                    key          TEXT NOT NULL,
                    value        TEXT NOT NULL,
                    PRIMARY KEY (modlist_name, key)
                )
                """
            )
            connection.execute(
                """
                INSERT INTO modlist_settings (modlist_name, key, value)
                VALUES (?, 'cache_only_mode', ?)
                ON CONFLICT(modlist_name, key) DO UPDATE SET value = excluded.value
                """,
                (modlist_name, "true" if cache_only else "false"),
            )
            connection.commit()
        finally:
            connection.close()

    # ---- env ------------------------------------------------------------------

    def launcher_env(self) -> dict[str, str]:
        """Env overrides that isolate the launcher to this workspace."""
        return {"XDG_DATA_HOME": str(self.xdg_data_home)}

    # ---- lifecycle ------------------------------------------------------------

    def cleanup(self) -> None:
        if not self._keep and self._tmp_root.exists():
            shutil.rmtree(self._tmp_root, ignore_errors=True)

    def __enter__(self) -> "HarnessWorkspace":
        return self

    def __exit__(self, *_exc) -> None:
        self.cleanup()


def _offline_uuid(username: str) -> str:
    """A stable, offline-style UUID string for display only.

    NOTE: this is NOT the launcher's own offline UUID (which is uuid v5 over a
    custom namespace, see offline_account.rs). It is only a placeholder for the
    profile_data display field; the launcher derives the authoritative uuid
    itself. Kept deterministic so seeded rows are reproducible.
    """
    import hashlib

    digest = hashlib.md5(f"OfflinePlayer:{username}".encode()).hexdigest()
    return f"{digest[0:8]}-{digest[8:12]}-{digest[12:16]}-{digest[16:20]}-{digest[20:32]}"


# Common fixtures -------------------------------------------------------------


def vanilla_modlist(name: str = "harness-vanilla") -> Modlist:
    """Empty modlist for Vanilla / no-mod launches."""
    return Modlist(name=name, description="Vanilla harness fixture", rules=[])


def h6_modlist(name: str = "harness-h6") -> Modlist:
    """H6 fixture: A<->B mutual requires with B disabled.

    strip_mutual_requires only strips the pair when BOTH are enabled
    (resolver.rs); with B disabled, A keeps requires:[B] and must NOT resolve as
    satisfied. Expressible purely in rules.json.
    """
    return Modlist(
        name=name,
        description="H6 mutual-requires-with-disabled-peer fixture",
        rules=[
            ModRule(mod_id="mod-a", requires=["mod-b"], enabled=True),
            ModRule(mod_id="mod-b", requires=["mod-a"], enabled=False),
        ],
    )
