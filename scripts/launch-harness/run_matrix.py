"""Dual-pass launch-verification matrix runner.

Mandated order (design):
  1. API pass — full matrix with the modlist default source ("ask Modrinth").
  2. Cache pass — runs ONLY if the API pass is entirely green. Flips each modlist
     to "prioritize downloaded jars" (cache_only) on the cache the API pass
     populated, in the SAME workspace, and verifies the same combos still launch.

For a negative fixture, "green" == the launch fails with the expected state.

Never aborts the matrix on a red combo: records state/durationMs/failureKind,
saves minecraftLogTail + launchLogDir, continues, and highlights reds in the
final summary. Emits an aggregated JSON report + a readable markdown table.

Timer compensation (design point b): successAfterSeconds starts BEFORE the
pipeline, so the first cold API pass may need a large value. successAfter is
parametrized per environment (default local 25s) and the effective wall
duration is recorded per combo.

stdlib-only.
"""

from __future__ import annotations

import argparse
import json
import time
from dataclasses import asdict
from pathlib import Path

from matrix import Combo, ci_matrix, full_local_matrix
from run_combo import ComboResult, ComboSpec, run_combo
from workspace import HarnessWorkspace, Modlist, h6_modlist, vanilla_modlist

# One modlist per loader; vanilla uses an empty list, modded loaders reuse it
# (no mods) for the base matrix. H6/S1 fixtures are separate, opt-in.
BASE_MODLIST = "harness-base"


def _spec_for(combo: Combo, mod_source: str, success_after: int, timeout: int) -> ComboSpec:
    return ComboSpec(
        loader=combo.loader,
        mc_version=combo.mc_version,
        modlist_name=BASE_MODLIST,
        mod_source=mod_source,
        timeout_seconds=timeout,
        success_after_seconds=success_after,
        expect_state=combo.expect_state,
    )


def _run_pass(
    combos: list[Combo],
    workspace: HarnessWorkspace,
    *,
    mod_source: str,
    display: str,
    binary: str | None,
    success_after: int,
    timeout: int,
) -> list[ComboResult]:
    results: list[ComboResult] = []
    for index, combo in enumerate(combos, start=1):
        spec = _spec_for(combo, mod_source, success_after, timeout)
        label = spec.label()
        print(f"[{mod_source}] ({index}/{len(combos)}) {label} ...", flush=True)
        try:
            result = run_combo(spec, workspace, binary=binary, display=display)
        except Exception as exc:  # never abort the matrix
            result = ComboResult(
                spec_label=label,
                passed=False,
                state="harness_exception",
                success=False,
                expected_state=combo.expect_state,
                duration_ms=0,
                duration_wall_s=0.0,
                failure_kind=None,
                failure_summary=None,
                launch_log_dir=None,
                harness_error=f"{type(exc).__name__}: {exc}",
            )
        verdict = "PASS" if result.passed else "FAIL"
        print(f"    -> {verdict} state={result.state} wall={result.duration_wall_s:.0f}s", flush=True)
        results.append(result)
    return results


def _markdown_report(
    api_results: list[ComboResult],
    cache_results: list[ComboResult] | None,
) -> str:
    lines: list[str] = ["# Launch-verification matrix report", ""]

    def table(title: str, results: list[ComboResult]) -> None:
        passed = sum(1 for r in results if r.passed)
        lines.append(f"## {title} — {passed}/{len(results)} green")
        lines.append("")
        lines.append("| combo | verdict | state | success | dur(ms) | wall(s) | failureKind |")
        lines.append("|---|---|---|---|---|---|---|")
        for r in results:
            verdict = "✅" if r.passed else "❌"
            lines.append(
                f"| {r.spec_label} | {verdict} | {r.state} | {r.success} "
                f"| {r.duration_ms} | {r.duration_wall_s:.0f} | {r.failure_kind or ''} |"
            )
        lines.append("")
        reds = [r for r in results if not r.passed]
        if reds:
            lines.append(f"### Reds in {title}")
            lines.append("")
            for r in reds:
                lines.append(f"- **{r.spec_label}** — state={r.state}, failureKind={r.failure_kind}")
                if r.harness_error:
                    lines.append(f"  - harnessError: {r.harness_error}")
                if r.launch_log_dir:
                    lines.append(f"  - launchLogDir: `{r.launch_log_dir}`")
                if r.minecraft_log_tail:
                    tail = "\\n".join(r.minecraft_log_tail[-15:])
                    lines.append(f"  - log tail (last 15):\n\n```\n{tail}\n```")
            lines.append("")

    table("API pass", api_results)
    if cache_results is not None:
        table("Cache pass", cache_results)
    else:
        lines.append("## Cache pass — SKIPPED (API pass was not all green)")
        lines.append("")
    return "\n".join(lines)


def run_matrix(
    *,
    scope: str,
    display: str,
    binary: str | None,
    success_after: int,
    timeout: int,
    report_dir: Path,
    keep: bool,
    workspace_path: str | None = None,
) -> bool:
    combos = ci_matrix() if scope == "ci" else full_local_matrix()
    report_dir.mkdir(parents=True, exist_ok=True)

    if workspace_path is not None:
        # Reuse an existing external data root (e.g. a hand-created instance):
        # never reset/zero it, seed only if the account is absent, and never
        # clobber existing modlists.
        ws_ctx = HarnessWorkspace(data_home=Path(workspace_path))
        reuse = True
    else:
        ws_ctx = HarnessWorkspace(keep=keep)
        reuse = False

    with ws_ctx as ws:
        mode = "REUSE (persistent)" if reuse else "fresh (temp)"
        print(f"[harness] workspace root: {ws.launcher_root}  [{mode}]")
        ws.seed_offline_account()  # idempotent: no-op if already seeded
        # Base modlists: never clobber a hand-created one in reuse mode.
        ws.create_modlist(vanilla_modlist(BASE_MODLIST), overwrite=not reuse)
        ws.create_modlist(h6_modlist(), overwrite=not reuse)

        started = time.monotonic()
        api_results = _run_pass(
            combos, ws, mod_source="api", display=display,
            binary=binary, success_after=success_after, timeout=timeout,
        )
        api_green = all(r.passed for r in api_results)

        cache_results: list[ComboResult] | None = None
        if api_green:
            print("[harness] API pass all green -> cache pass")
            ws.set_modlist_cache_only(BASE_MODLIST, True)
            cache_results = _run_pass(
                combos, ws, mod_source="cache", display=display,
                binary=binary, success_after=success_after, timeout=timeout,
            )
        else:
            print("[harness] API pass has reds -> cache pass SKIPPED")

        wall = time.monotonic() - started

    aggregate = {
        "scope": scope,
        "display": display,
        "successAfterSeconds": success_after,
        "timeoutSeconds": timeout,
        "wallSeconds": round(wall, 1),
        "apiPass": {
            "green": api_green,
            "results": [r.to_json() for r in api_results],
        },
        "cachePass": (
            {"results": [r.to_json() for r in cache_results]}
            if cache_results is not None
            else {"skipped": True}
        ),
    }
    (report_dir / "matrix-report.json").write_text(json.dumps(aggregate, indent=2) + "\n")
    (report_dir / "matrix-report.md").write_text(
        _markdown_report(api_results, cache_results)
    )

    api_reds = [r.spec_label for r in api_results if not r.passed]
    cache_reds = (
        [r.spec_label for r in cache_results if not r.passed] if cache_results else []
    )
    print("\n===== SUMMARY =====")
    print(f"API pass: {len(api_results) - len(api_reds)}/{len(api_results)} green")
    if api_reds:
        print("  reds: " + ", ".join(api_reds))
    if cache_results is not None:
        print(f"Cache pass: {len(cache_results) - len(cache_reds)}/{len(cache_results)} green")
        if cache_reds:
            print("  reds: " + ", ".join(cache_reds))
    else:
        print("Cache pass: SKIPPED (API not all green)")
    print(f"Reports: {report_dir / 'matrix-report.md'}")

    return api_green and not cache_reds


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Run the dual-pass launch matrix.")
    parser.add_argument("--scope", choices=["ci", "local"], default="local")
    parser.add_argument("--display", choices=["xvfb", "native"], default="native")
    parser.add_argument("--binary", default=None)
    parser.add_argument("--success-after", type=int, default=25,
                        help="local default 25s; raise for cold first API pass")
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--report-dir", default="scripts/launch-harness/reports")
    parser.add_argument("--keep", action="store_true")
    parser.add_argument(
        "--workspace",
        default=None,
        help="reuse an existing persistent data root (XDG_DATA_HOME); seed only if "
        "absent, never reset/clobber. e.g. ~/cubic-harness-data",
    )
    args = parser.parse_args()

    ok = run_matrix(
        scope=args.scope,
        display=args.display,
        binary=args.binary,
        success_after=args.success_after,
        timeout=args.timeout,
        report_dir=Path(args.report_dir),
        keep=args.keep,
        workspace_path=args.workspace,
    )
    raise SystemExit(0 if ok else 1)
