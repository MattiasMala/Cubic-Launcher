<div align="center"><img src="assets/icon.png" width="180" alt="Cubic Launcher" /></div>

# Cubic Launcher

**A rule-based Minecraft launcher and modlist manager.**

<p align="center">
  <a href="LICENSE"><img alt="License: GPLv3" src="https://img.shields.io/badge/License-GPLv3-blue.svg"></a>
  <img alt="Tauri 2" src="https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white">
  <img alt="SolidJS" src="https://img.shields.io/badge/SolidJS-1.9-2C4F7C?logo=solid&logoColor=white">
  <img alt="TypeScript" src="https://img.shields.io/badge/TypeScript-5-3178C6?logo=typescript&logoColor=white">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-backend-000000?logo=rust&logoColor=white">
  <img alt="Platform: Windows and Linux" src="https://img.shields.io/badge/Platform-Windows%20%7C%20Linux-7c3aed">
</p>

Build, maintain, and launch rule-driven Minecraft modlists with explicit choices, reusable local caches, and Modrinth integration.

## Dependency philosophy

**You manage dependencies.** When you add any mod—including a dependency another mod needs—the launcher selects the latest release tagged for the exact Minecraft version and loader in play (a beta or alpha only when there is no release). The one exception is a mod you pin: **Pin** fixes it to the release you pick for one Minecraft version and loader, and the other targets keep following the latest release unless you also remove the unpinned entry. Cubic Launcher never inspects what a mod declares it needs, never auto-downloads dependencies behind your back, and never auto-excludes mods. If a dependency really is missing, the Minecraft client says so at startup.

- Deterministic, user-controlled modlists.
- No hidden downloads or surprise version drift.

## Supported versions

- Minecraft **1.6.4 and later**
- **Vanilla, Fabric, Forge, and NeoForge**, wherever the loader publishes a build for that Minecraft version (Fabric from 1.14, NeoForge from 1.20.1)
- **Windows and Linux**

An automated launch matrix (`scripts/launch-harness/`) starts the game without mods, on Linux, at the versions where the launch code takes a different path: 1.6.4, 1.7.10, 1.8.9, 1.12.2, 1.13.2, 1.14.4, 1.16.5, 1.17.1, 1.18.2, 1.19.4, 1.20.1, 1.20.4, and 1.20.5, each with the loaders available for it. It runs weekly as an advisory smoke check, and a local sweep adds the 1.15, 1.19, 1.20.6, and 1.21 bands. The versions in between share their code path with a tested neighbour; they are not launched one by one, and mods are not part of the matrix.

## Playing offline

An account set to **Play offline** in Manage Accounts plays from what is already on disk, and the launcher makes no network requests for it: no Modrinth checks or searches, no update check, and the Skins screen shows your local library only. Local resource packs, data packs, and shaders are installed; Modrinth ones are skipped. A Minecraft version that was never downloaded, or a loader that was never launched online, is an error that says so, not a download, and Java falls back to the nearest higher major already installed.

If refreshing a Microsoft sign-in fails at launch, the game starts offline with a visible notice; a sign-in Microsoft no longer accepts asks you to **Sign in again**.

## Your worlds and content

Data packs are linked into the `datapacks/` folder of every world of the instance, shared worlds included, at every launch. A world you create in game receives them from the next launch.

**Delete Mod-list** tells you how many worlds and screenshots the modlist holds, refuses while its game is running, and moves the folder to the system trash, so a deletion can be undone from there.

## Java runtimes

Cubic Launcher manages Java runtimes automatically. If the exact required Java major is unavailable, it uses the nearest higher available major: from Adoptium when online, among the runtimes already installed when playing offline.

## Install

Download the latest build from [GitHub Releases](https://github.com/MattiasMala/Cubic-Launcher/releases). Cubic Launcher shows in-app update notifications and downloads and installs an update only after you confirm.

## Quick start for development

Requires **Rust**, **Node.js**, and the platform prerequisites for Tauri 2.

```sh
git clone https://github.com/MattiasMala/Cubic-Launcher.git
cd Cubic-Launcher
npm install
npm run tauri dev
```

## Contributing

See [Architecture and development guide](docs/ARCHITECTURE.md) for the data model, backend and frontend structure, storage, authentication, and release process.

## License

Cubic Launcher is distributed under the [GNU General Public License v3.0](LICENSE).
