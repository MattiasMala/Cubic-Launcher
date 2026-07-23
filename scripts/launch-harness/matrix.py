"""Declarative launch-verification matrix (stratified, from the Phase-1 design).

Two strata:
- Stratum A ("ci"): boundary versions that exercise a distinct CODE PATH — the
  minimal subset a CI advisory run should cover. Each carries the reason.
- Stratum B ("local"): the rest — same code path as their band, run in the full
  local sweep. A+B together = the complete local matrix.

Plus negative fixtures (expected-incompatibility): PASS == the launch fails with
the expected state, never in-world.

Loaders: vanilla / fabric / forge / neoforge.
Boundary rationale references (see launch-harness-design.md):
- args legacy vs modern: minecraft_downloader.rs:723-734 (pre-1.13)
- Java major: java_runtime.rs:200-231 (<=1.16=>8, 1.17=>16, 1.18/19+1.20.0-4=>17,
  1.20.5+/1.21=>21)
- Fabric/Forge/NeoForge availability = presence of metadata (not a hard-coded range)

stdlib-only; pure data.
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass(frozen=True)
class MatrixVersion:
    mc_version: str
    stratum: str  # "ci" (boundary) | "local"
    reason: str
    # Loaders to exercise for this version (availability-aware).
    loaders: tuple[str, ...] = ("vanilla", "fabric", "forge", "neoforge")


# Stratum A — boundary versions for CI.
STRATUM_A: list[MatrixVersion] = [
    MatrixVersion("1.12.2", "ci", "last legacy minecraftArguments + Java 8; wrapper legacy (Forge)",
                  loaders=("vanilla", "forge")),
    MatrixVersion("1.13.2", "ci", "first modern arguments.game/jvm; Fabric negative side handled as fixture",
                  loaders=("vanilla", "forge")),
    MatrixVersion("1.14.4", "ci", "first Fabric-supported side", loaders=("vanilla", "fabric", "forge")),
    MatrixVersion("1.16.5", "ci", "last Java 8 band"),
    MatrixVersion("1.17.1", "ci", "Java 8->16 boundary"),
    MatrixVersion("1.18.2", "ci", "Java 16->17 boundary"),
    MatrixVersion("1.19.4", "ci", "NeoForge negative side handled as fixture",
                  loaders=("vanilla", "fabric", "forge")),
    MatrixVersion("1.20.1", "ci", "first NeoForge-supported side (net.neoforged Prism + installer)"),
    MatrixVersion("1.20.4", "ci", "last 1.20 on Java 17"),
    MatrixVersion("1.20.5", "ci", "first Java 21 (boundary is patch 5, not 1.20.6)"),
]

# Stratum B — same code path as their band; local full sweep only.
STRATUM_B: list[MatrixVersion] = [
    MatrixVersion("1.15.2", "local", "modern args / Java 8 band"),
    MatrixVersion("1.19.2", "local", "modern args / Java 17 band"),
    MatrixVersion("1.20.6", "local", "Java 21 band"),
    MatrixVersion("1.21.1", "local", "Java 21 / 1.21 band"),
    MatrixVersion("1.21.4", "local", "Java 21 / 1.21 band"),
    MatrixVersion("1.21.5", "local", "Java 21 / 1.21 band"),
    MatrixVersion("1.21.8", "local", "Java 21 / 1.21 band"),
    MatrixVersion("1.21.11", "local", "Java 21 / 1.21 band [existence to confirm at run time]"),
]


@dataclass(frozen=True)
class NegativeFixture:
    loader: str
    mc_version: str
    expect_state: str
    reason: str


# Expected-incompatibility fixtures: PASS == the launch fails as expected.
NEGATIVE_FIXTURES: list[NegativeFixture] = [
    NegativeFixture("vanilla", "1.6.4", "launch_failed",
                    "asset-index legacy format; parser requires assetIndex, no legacy branch"),
    NegativeFixture("fabric", "1.13.2", "launch_failed",
                    "Fabric not available before 1.14 -> metadata fetch fails"),
    NegativeFixture("neoforge", "1.19.4", "launch_failed",
                    "NeoForge not available before 1.20.1 -> metadata fetch fails"),
]


@dataclass
class Combo:
    loader: str
    mc_version: str
    stratum: str
    reason: str
    expect_state: str | None = None  # set for negative fixtures


def positive_combos(strata: tuple[str, ...] = ("ci", "local")) -> list[Combo]:
    """All (loader, version) combos for the requested strata, availability-aware."""
    out: list[Combo] = []
    for version in STRATUM_A + STRATUM_B:
        if version.stratum not in strata:
            continue
        for loader in version.loaders:
            out.append(Combo(loader, version.mc_version, version.stratum, version.reason))
    return out


def negative_combos() -> list[Combo]:
    return [
        Combo(f.loader, f.mc_version, "negative", f.reason, expect_state=f.expect_state)
        for f in NEGATIVE_FIXTURES
    ]


def ci_matrix() -> list[Combo]:
    """Advisory CI: stratum A positives + negative fixtures."""
    return positive_combos(("ci",)) + negative_combos()


def full_local_matrix() -> list[Combo]:
    """Full local sweep: A+B positives + negative fixtures."""
    return positive_combos(("ci", "local")) + negative_combos()


if __name__ == "__main__":
    import json

    print("== CI matrix (stratum A + negatives) ==")
    for c in ci_matrix():
        tag = f" expect={c.expect_state}" if c.expect_state else ""
        print(f"  {c.loader}@{c.mc_version} [{c.stratum}]{tag}  # {c.reason}")
    print(f"\nCI combos: {len(ci_matrix())}")
    print(f"Full local combos: {len(full_local_matrix())}")
