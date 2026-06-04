<div align="center">

<pre>
   ____      _     _        _                           _
  / ___|   _| |__ (_) ___  | |    __ _ _   _ _ __   ___| |__   ___ _ __
 | |  | | | | '_ \| |/ __| | |   / _` | | | | '_ \ / __| '_ \ / _ \ '__|
 | |__| |_| | |_) | | (__  | |__| (_| | |_| | | | | (__| | | |  __/ |
  \____\__,_|_.__/|_|\___| |_____\__,_|\__,_|_| |_|\___|_| |_|\___|_|
</pre>

<strong>Rule-based Minecraft mod-list launcher for building, resolving, caching, and launching curated packs.</strong>

<p>Tauri 2 · SolidJS · TypeScript · Rust · Modrinth integration · local-first cache</p>

<p>
  <a href="LICENSE"><img alt="License: GPLv3" src="https://img.shields.io/badge/License-GPLv3-blue.svg"></a>
  <img alt="Tauri 2" src="https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white">
  <img alt="SolidJS" src="https://img.shields.io/badge/SolidJS-1.9-2C4F7C?logo=solid&logoColor=white">
  <img alt="TypeScript" src="https://img.shields.io/badge/TypeScript-5-3178C6?logo=typescript&logoColor=white">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-backend-000000?logo=rust&logoColor=white">
  <img alt="Platform" src="https://img.shields.io/badge/Platform-desktop-7c3aed">
</p>

<p>
  <a href="#what-the-app-does">What it does</a> ·
  <a href="#architecture">Architecture</a> ·
  <a href="#database">Database</a> ·
  <a href="#local-development">Development</a> ·
  <a href="#license">License</a>
</p>

</div>

## Project status

- Stack: **Tauri 2 + SolidJS + TypeScript + Rust**
- Primary desktop target: Windows, with a standard Tauri project structure for other OSes as well
- Loader support in the resolver: **Fabric, NeoForge, Forge, Vanilla**
- Microsoft login is implemented in the backend with OAuth 2.0 + PKCE, Xbox Live, XSTS, and Minecraft profile exchange

## What the app does

- Loads an initial shell with mod lists, the active account, global settings, and per-mod-list overrides
- Opens a mod list in a hierarchical editor with:
  - include/exclude rules
  - nested alternatives
  - incompatibilities with explicit winner selection
  - directional links between mods
  - functional tags and visual groups
  - Minecraft version and loader constraints
  - custom config file mappings
- Manages non-mod content too:
  - resource packs
  - data packs
  - shader packs
- Resolves the active set for a **Minecraft version + mod loader** pair
- Queries Modrinth, resolves dependencies, checks compatibility, downloads artifacts, and reuses cache when possible
- Prepares the launch instance, links or copies mods and configs, builds the JVM command, and starts Minecraft
- Streams progress, launch logs, errors, and process exit events back to the frontend
- Exports a mod list as a zip archive with metadata and, optionally, artifacts and extra files

## Mod list data model

The backend stores rules in `rules.json`.
Each `Rule` can contain:

- `mod_id`
- `source` (`modrinth` or `local`)
- `enabled`
- `exclude_if`
- `requires`
- `version_rules`
- `custom_configs`
- `alternatives`

This allows a single mod list to represent fallbacks, conflicts, and variants without duplicating separate lists.

## Architecture

### Frontend (`src/`)

SolidJS frontend with a unidirectional data flow:

- `App.tsx` orchestrates layout, modals, and backend calls
- `store-state.ts` contains raw signals
- `store-selectors.ts` contains derived state only
- `store-actions.ts` contains reusable mutations
- `app/use-app-bootstrap.ts` performs initial bootstrap and registers Tauri listeners
- `app/persistence-effects.ts` reactively syncs durable changes back to the backend
- `app/backend-loaders.ts` is the main bridge to Tauri commands
- `lib/dragEngine.ts` implements a custom drag-and-drop engine designed to avoid layout thrashing

The frontend can also run with `npm run dev` in browser mode for UI iteration, but many features remain **desktop-only** and are guarded by `isTauri()` checks.

### Backend (`src-tauri/src/`)

Rust/Tauri backend, which is the source of truth for durable state and system operations:

- `lib.rs` registers the Tauri command surface
- `app_shell.rs` loads and saves shell snapshots, the active account, and settings
- `rules.rs` defines the rules format
- `resolver.rs` resolves a mod list for a Minecraft/loader target
- `dependencies.rs` resolves transitive dependencies
- `modrinth.rs` integrates the Modrinth API
- `mod_cache.rs` and `database.rs` handle cache state and SQLite persistence
- `modlist_manager.rs` creates/imports mod lists and copies local JARs
- `modlist_assets.rs` manages presentation, editor groups, zip export, and instance file browsing
- `content_packs.rs` manages resource packs, data packs, and shaders
- `minecraft_downloader.rs`, `loader_metadata.rs`, `java_runtime.rs`, and `adoptium.rs` manage Minecraft assets, loader metadata, and Java runtimes
- `launch_preview.rs` and the `launch_preview_*` files orchestrate the launch and verification pipeline
- `token_storage.rs` encrypts tokens with **AES-256-GCM** and stores the key in the OS keyring

## Launch pipeline

At a high level, the backend:

1. loads the shell snapshot, account, and overrides
2. resolves active mods and dependencies
3. checks local cache and remote availability
4. downloads missing Minecraft assets, loaders, mods, and dependencies
5. prepares the instance directory, `mods/`, and `config/`
6. builds the Java command with RAM settings, custom arguments, profiler, and optional wrapper
7. starts the process and emits logs/progress back to the frontend

## Local storage

Tauri initializes everything under the app's local data directory. The structure managed by `launcher_paths.rs` includes:

- `launcher_data.db`
- `cache/`
  - `mods/`
  - `configs/`
  - `content-packs/`
- `logs/`
  - `launches/`
- `mod-lists/`
- `java-runtimes/`

Inside a single mod list, the main files are:

- `rules.json`
- `modlist-presentation.json`
- `modlist-editor-groups.json`
- `resourcepacks.json`
- `datapacks.json`
- `shaders.json`

## Database

The application uses a local SQLite database: `launcher_data.db`.

`database.rs` initializes and migrates these main tables:

- `accounts` — Microsoft/Xbox/Minecraft account records and encrypted token payload references
- `java_installations` — discovered or user-provided Java runtimes, with version and architecture
- `mod_cache` — cached mod artifacts keyed by Modrinth version and launch target
- `modrinth_project_aliases` — slug/canonical-project-id mapping used by cache resolution
- `dependencies` — resolved dependency relationships between mods
- `config_attribution` — records linking generated config files back to the originating JAR
- `global_settings` — launcher-wide persisted settings
- `modlist_settings` — per-mod-list overrides
- `modrinth_availability` — cached compatibility/availability lookups by project, Minecraft version, and loader

The database stores launcher metadata and cache state. Actual rule trees, mod list presentation, editor groups, and content pack lists remain file-based inside each mod list directory.

## Repository layout

- `src/` — SolidJS frontend
- `src/app/` — bootstrap, reactive persistence, row-state mapping, backend loaders
- `src/components/` — UI components and feature-specific subsections
- `src/lib/` — shared types, drag engine, logger, and frontend tracing
- `src-tauri/src/` — Rust/Tauri backend
- `src-tauri/src/launch_preview*.rs` — launch pipeline split by responsibility
- `src-tauri/src/editor_data*.rs` — editor command surface, models, and tests

## Development prerequisites

You need:

- **Node.js LTS**
- **Rust**
- the system prerequisites required by **Tauri 2**
<https://v2.tauri.app/start/prerequisites/>

## Local development

Install dependencies:

```sh
npm install
```

Run the Tauri desktop app:

```sh
npm run tauri dev
```

Checks when you touch both frontend and backend:

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

## License

This project is distributed under **GNU General Public License v3.0**. See `LICENSE`.
