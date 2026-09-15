# Architecture and development

Cubic Launcher is a local-first, rule-based Minecraft launcher and modlist manager built with Tauri 2, SolidJS, TypeScript, and Rust.

## Project status

- Verified desktop targets: **Windows and Linux**
- Verified Minecraft range: **1.6.4 through the latest release**
- Verified loaders: **Vanilla, Fabric, Forge, and NeoForge**
- An automated launch matrix performs real launch smoke runs, including Linux runs under Xvfb.
- Microsoft sign-in uses OAuth 2.0 Authorization Code Flow with PKCE, followed by Xbox Live, XSTS, and Minecraft profile exchange.

## What the app does

- Loads the modlists, active account, global settings, and per-modlist overrides into the application shell.
- Provides a hierarchical modlist editor with:
  - include and exclude rules
  - nested alternatives
  - incompatibilities with explicit winner selection
  - directional links between mods
  - functional tags and visual groups
  - Minecraft version and loader constraints
  - custom config file mappings
- Manages resource packs, data packs, and shader packs alongside mods.
- Evaluates the active rule set for a Minecraft version and loader target.
- Queries Modrinth for explicitly selected remote content and reuses locally cached artifacts where possible.
- Prepares an isolated launch instance, assembles the JVM command, starts Minecraft, and streams progress, logs, errors, and process-exit events to the frontend.
- Exports a modlist as a zip archive with metadata and optional artifacts and extra files.

## Rule and dependency behavior

`resolver.rs` evaluates the user-authored rule tree. Enabled state, `exclude_if`, `requires`, version rules, and nested alternatives determine the active set for a target. The `requires` field is an explicit relationship inside the modlist; it is separate from dependencies declared in Modrinth or JAR metadata.

For each explicitly selected Modrinth project, Cubic Launcher chooses the latest release tagged for the exact Minecraft version in play. Only content represented by the selected rules is acquired.

Dependencies are the user's to manage, and the launcher does not look at them. Required-dependency declarations in Modrinth metadata and in the `fabric.mod.json` embedded in a JAR are not inspected, not reported, and never trigger a download, a version pin, or the exclusion of the requiring mod. A dependency that is genuinely missing surfaces where it is known exactly: in the Minecraft client at startup.

## Modlist data model

The backend stores each modlist's rules in `rules.json`. The current on-disk schema is version 4 and includes the modlist name, author, description, and a list of `Rule` values.

Each `Rule` contains:

- `mod_id`
- `source` (`modrinth` or `local`)
- `enabled`
- `exclude_if`
- `requires`
- `version_rules`
- `custom_configs`
- `alternatives`

This tree represents target-specific variants, fallbacks, and conflicts without duplicating whole modlists. Presentation data, editor groups, and non-mod content lists are stored separately from the rule tree.

## Application architecture

### Frontend (`src/`)

The SolidJS frontend follows a unidirectional data flow:

- `App.tsx` orchestrates the main layout, modals, and backend interactions.
- `store-state.ts` owns raw signals.
- `store-selectors.ts` owns derived state.
- `store-actions.ts` contains reusable mutations.
- `app/use-app-bootstrap.ts` performs initial loading and registers Tauri event listeners.
- `app/persistence-effects.ts` reactively persists durable changes through backend commands.
- `app/backend-loaders.ts` is the main bridge to the Tauri command surface.
- `lib/dragEngine.ts` implements custom drag-and-drop behavior while avoiding layout thrashing.

`npm run dev` runs the frontend in a browser for UI iteration. System integration remains desktop-only and is guarded by `isTauri()` checks.

### Backend (`src-tauri/src/`)

The Rust backend is the source of truth for durable state and system operations:

- `lib.rs` initializes application state, plugins, and Tauri commands.
- `app_shell.rs` loads and saves shell snapshots, account selection, and settings.
- `rules.rs` defines and validates the modlist rule format.
- `resolver.rs` evaluates rules for a Minecraft/loader target.
- `modrinth.rs` queries Modrinth and selects target-compatible versions.
- `mod_cache.rs`, `launch_preview_cache.rs`, and `database.rs` manage cached artifacts and SQLite state.
- `modlist_manager.rs` creates and imports modlists and copies user-selected local JARs.
- `modlist_assets.rs` manages presentation data, editor groups, zip export, and instance-file browsing.
- `content_packs.rs` manages resource packs, data packs, and shader packs.
- `minecraft_downloader.rs`, `loader_metadata.rs`, `java_runtime.rs`, and `adoptium.rs` manage Minecraft assets, loader metadata, and Java runtimes.
- `launch_preview.rs` and the `launch_preview_*` modules orchestrate launch preparation and verification.
- `microsoft_auth.rs`, `account_manager.rs`, and `token_storage.rs` implement account login and credential persistence.

## Launch pipeline

At a high level, the backend:

1. Loads the shell snapshot and combines global settings with per-modlist overrides.
2. Evaluates explicit rules and alternatives for the requested Minecraft version and loader.
3. Chooses target-tagged versions for the selected remote projects and checks the cache.
4. Downloads missing artifacts for that selected content plus the required Minecraft, loader, and content-pack assets.
5. Prepares the target-specific instance directories, mods, configs, libraries, natives, and launch metadata.
6. Selects or downloads a suitable Java runtime, refreshes the Minecraft session when possible, and builds the JVM command with memory settings, custom arguments, profiler, and optional wrapper.
7. Starts Minecraft and emits progress and process output to the frontend while writing a per-launch log session.

Java runtimes are auto-managed. The launcher first looks for the exact required major; when that major is unavailable from Adoptium, it walks upward and uses the nearest available higher major while surfacing a notice.

## Authentication and credential storage

Microsoft login opens the system browser, receives the OAuth redirect on a loopback listener, validates the OAuth state, and exchanges the authorization code using PKCE. The resulting Microsoft token feeds the Xbox Live, XSTS, and Minecraft authentication chain.

Current account writes encrypt access and refresh tokens with AES-256-GCM before storing the ciphertext in the `access_token_enc` and `refresh_token_enc` SQLite columns. A random 256-bit encryption key is generated once and stored through the operating-system keyring under service `com.cubic.launcher`; each encrypted payload uses a fresh 12-byte nonce and carries a format version byte.

`profile_data` is unencrypted and current writers restrict it to non-sensitive display metadata such as username and UUID. A startup migration removes tokens left there by older builds after preserving the refresh token in encrypted storage. If the keyring is unavailable and a legacy plaintext refresh token is the only recoverable copy, migration deliberately defers its removal rather than destroy the credential.

## Local storage

Tauri initializes launcher state below the application's local data directory. `launcher_paths.rs` manages this layout:

- `launcher_data.db`
- `cache/`
  - `minecraft/`
  - `mods/`
  - `configs/`
  - `content-packs/`
- `logs/launches/`
- `mod-lists/`
- `java-runtimes/`

A modlist can contain:

- `rules.json`
- `modlist-presentation.json`
- `modlist-editor-groups.json`
- `resourcepacks.json`
- `datapacks.json`
- `shaders.json`
- `local-jars/` — JARs uploaded for local mod rules
- `resourcepacks/`, `datapacks/`, `shaders/` — locally imported content packs, one copy per modlist rather than one per instance target. Each is either a `.zip` or an unpacked directory, and instances link from here.
- `.cubic/icons/<category>/<pack>.png` — the `pack.png` extracted from each locally imported pack at import time, so rendering a list never reopens an archive. The entry in the category JSON stores this path, not the image bytes.
- `instances/<minecraft-version>-<loader>/`
  - `.cubic/managed-resourcepacks.json`, `.cubic/managed-shaderpacks.json`, `.cubic/managed-datapacks.json` — the content packs the launcher linked into that instance. The next launch removes only the entries it recorded and no longer wants; anything else in `resourcepacks/`, `shaderpacks/` and `datapacks/` is left untouched, and a missing or unreadable manifest removes nothing.

## Database

The local SQLite database is `launcher_data.db`. `database.rs` initializes and migrates these tables:

- `accounts` — account identity, active-account state, encrypted token blobs, and non-sensitive profile metadata
- `java_installations` — discovered or user-provided Java runtimes with major version and architecture
- `mod_cache` — cached remote or local artifact metadata keyed by Modrinth version and launch target
- `modrinth_project_aliases` — slug-to-canonical-project-ID mappings used by cache lookups
- `dependencies` — unused legacy dependency-link table; nothing writes it and nothing reads it
- `config_attribution` — generated config paths linked to their originating JARs
- `global_settings` — launcher-wide settings
- `modlist_settings` — per-modlist overrides
- `modrinth_availability` — cached project availability by Minecraft version and loader

The database contains launcher metadata and cache state. Rule trees, presentation, editor groups, content-pack lists, and prepared instances remain file-based in each modlist directory.

## Repository layout

- `src/` — SolidJS frontend
- `src/app/` — bootstrap, reactive persistence, row-state mapping, and backend loaders
- `src/components/` — UI and feature-specific components
- `src/lib/` — shared types, drag engine, logging, and frontend tracing
- `src-tauri/src/` — Rust/Tauri backend
- `src-tauri/src/launch_preview*.rs` — launch pipeline split by responsibility
- `src-tauri/src/editor_data*.rs` — editor commands, models, and tests
- `scripts/launch-harness/` — automated real-launch matrix tooling
- `.github/workflows/` — launch-matrix and release automation

## Development prerequisites

Install:

- **Node.js LTS**
- **Rust**
- the [Tauri 2 system prerequisites](https://v2.tauri.app/start/prerequisites/)

## Local development

Install dependencies and run the desktop app:

```sh
npm install
npm run tauri dev
```

For browser-only UI iteration:

```sh
npm run dev
```

Checks for changes spanning frontend and backend:

```sh
npx tsc --noEmit
cargo check --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
```

Desktop build:

```sh
npm run build
npm run tauri build
```

## Versioning

The version is recorded in `package.json`, `src-tauri/Cargo.toml`, and `src-tauri/tauri.conf.json`; all three **must be bumped in lockstep**. `src-tauri/tauri.conf.json` is the value Tauri bundles and the updater compares against, so it is the source of truth for releases.

## Release and updates

Tags matching `v*` trigger `.github/workflows/release.yml`. The workflow builds Windows and Linux bundles with `tauri-apps/tauri-action`, creates a draft GitHub release, and produces the signed updater manifest `latest.json`.

The in-app updater checks `https://github.com/MattiasMala/Cubic-Launcher/releases/latest/download/latest.json`. It reports available releases in the UI and downloads, installs, and relaunches only after the user confirms.

## License

Cubic Launcher is distributed under the [GNU General Public License v3.0](../LICENSE).
