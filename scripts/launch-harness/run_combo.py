"""Run a single (loader, mc_version, mod_source) combo through the launcher's
automation verification entry-point and validate the resulting verification.json.

Contract (feat(automation), commit 3fc125d):
- CUBIC_AUTOMATION_VERIFY_REQUEST = JSON LaunchVerificationRequest (camelCase:
  modlistName / minecraftVersion / modLoader required; timeoutSeconds default 45,
  successAfterSeconds default 15, terminateOnSuccess/terminateOnTimeout default true).
- CUBIC_AUTOMATION_VERIFY_OUTPUT = path where the full LaunchVerificationResult
  JSON is written.
- CUBIC_AUTOMATION_VERIFY_EXIT=1 => the process always exits when verification
  concludes (0 on success, 1 otherwise); terminate_on_* close Minecraft first.

Display strategy:
- "xvfb"   : wrap the binary in `xvfb-run -a` (CI / headless default).
- "native" : use the caller's existing DISPLAY (local checkpoint with a real GPU;
             also the only option where xvfb is not installed).

stdlib-only.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import time
from dataclasses import dataclass, field
from pathlib import Path

from workspace import HarnessWorkspace

# The 5 verifier states + the automation-only error state.
VERIFIER_STATES = {"launch_failed", "crashed", "exited", "running", "timed_out"}
AUTOMATION_ERROR_STATE = "automation_error"


@dataclass
class ComboSpec:
    loader: str  # "vanilla" | "fabric" | "forge" | "neoforge"
    mc_version: str
    modlist_name: str
    mod_source: str = "api"  # "api" | "cache" (report label only)
    timeout_seconds: int = 600
    success_after_seconds: int = 25
    terminate_on_success: bool = True
    terminate_on_timeout: bool = True
    # For negative fixtures: the state that counts as a PASS.
    expect_state: str | None = None  # e.g. "launch_failed"

    def request_json(self) -> dict:
        return {
            "modlistName": self.modlist_name,
            "minecraftVersion": self.mc_version,
            "modLoader": self.loader,
            "timeoutSeconds": self.timeout_seconds,
            "successAfterSeconds": self.success_after_seconds,
            "terminateOnSuccess": self.terminate_on_success,
            "terminateOnTimeout": self.terminate_on_timeout,
        }

    def label(self) -> str:
        return f"{self.loader}@{self.mc_version}[{self.mod_source}]"


@dataclass
class ComboResult:
    spec_label: str
    passed: bool
    state: str
    success: bool
    expected_state: str | None
    duration_ms: int
    duration_wall_s: float
    failure_kind: str | None
    failure_summary: str | None
    launch_log_dir: str | None
    minecraft_log_tail: list[str] = field(default_factory=list)
    harness_error: str | None = None  # harness-level failure (not the launcher's)

    def to_json(self) -> dict:
        return {
            "combo": self.spec_label,
            "passed": self.passed,
            "state": self.state,
            "success": self.success,
            "expectedState": self.expected_state,
            "durationMs": self.duration_ms,
            "durationWallSeconds": round(self.duration_wall_s, 1),
            "failureKind": self.failure_kind,
            "failureSummary": self.failure_summary,
            "launchLogDir": self.launch_log_dir,
            "minecraftLogTail": self.minecraft_log_tail,
            "harnessError": self.harness_error,
        }


def _resolve_binary(explicit: str | None) -> Path:
    if explicit:
        path = Path(explicit)
        if not path.exists():
            raise FileNotFoundError(f"launcher binary not found: {path}")
        return path
    # Prefer release, fall back to debug.
    here = Path(__file__).resolve().parents[2]  # repo root
    for candidate in (
        here / "src-tauri/target/release/cubic_launcher",
        here / "src-tauri/target/debug/cubic_launcher",
    ):
        if candidate.exists():
            return candidate
    raise FileNotFoundError(
        "no cubic_launcher binary found; build with `cargo build` (or --release) first"
    )


def _build_command(binary: Path, display: str) -> list[str]:
    if display == "xvfb":
        if shutil.which("xvfb-run") is None:
            raise RuntimeError(
                "display='xvfb' but xvfb-run is not installed "
                "(install xorg-server-xvfb / xvfb, or use display='native')"
            )
        # -a picks a free server number; -s configures a 24-bit screen.
        return ["xvfb-run", "-a", "-s", "-screen 0 1280x720x24", str(binary)]
    if display == "native":
        return [str(binary)]
    raise ValueError(f"unknown display strategy: {display!r}")


def run_combo(
    spec: ComboSpec,
    workspace: HarnessWorkspace,
    *,
    binary: str | None = None,
    display: str = "xvfb",
    external_margin_s: int = 120,
    reset_db_before: bool = False,
) -> ComboResult:
    """Launch one combo and return the validated result.

    The launcher writes verification.json to CUBIC_AUTOMATION_VERIFY_OUTPUT; we
    also enforce an external safety timeout of timeoutSeconds + margin so a hung
    process is always killed.

    reset_db_before defaults to False: the DB is seeded ONCE at workspace setup,
    and the cache pass relies on modlist_settings (cache_only) + accumulated
    mod_cache surviving across launches. Resetting per-launch would wipe both.
    (The launcher still re-runs its token migration at every startup, so the
    token-free security property holds regardless.)
    """
    binary_path = _resolve_binary(binary)
    output_path = workspace.launcher_root / f"verify-{spec.loader}-{spec.mc_version}.json"
    if output_path.exists():
        output_path.unlink()

    # Opt-in only: fresh, token-free DB for this launch (re-seed to keep an
    # active account). Off by default so cache/settings persist across the matrix.
    if reset_db_before:
        workspace.reset_database()
        workspace.seed_offline_account()

    env = dict(os.environ)
    env.update(workspace.launcher_env())
    env["CUBIC_AUTOMATION_VERIFY_REQUEST"] = json.dumps(spec.request_json())
    env["CUBIC_AUTOMATION_VERIFY_OUTPUT"] = str(output_path)
    env["CUBIC_AUTOMATION_VERIFY_EXIT"] = "1"
    # Force X11 backend so the launcher (and Minecraft) target the X display
    # instead of Wayland, matching xvfb / XWayland setups deterministically.
    env.setdefault("GDK_BACKEND", "x11")

    command = _build_command(binary_path, display)
    external_timeout = spec.timeout_seconds + external_margin_s

    started = time.monotonic()
    harness_error: str | None = None
    try:
        subprocess.run(
            command,
            env=env,
            timeout=external_timeout,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
    except subprocess.TimeoutExpired:
        harness_error = (
            f"external safety timeout: process exceeded {external_timeout}s "
            f"(timeoutSeconds={spec.timeout_seconds} + margin {external_margin_s}s)"
        )
    wall = time.monotonic() - started

    # Read the verifier's output.
    if not output_path.exists():
        return ComboResult(
            spec_label=spec.label(),
            passed=False,
            state="no_output",
            success=False,
            expected_state=spec.expect_state,
            duration_ms=0,
            duration_wall_s=wall,
            failure_kind=None,
            failure_summary=None,
            launch_log_dir=None,
            harness_error=harness_error
            or "launcher produced no verification.json at CUBIC_AUTOMATION_VERIFY_OUTPUT",
        )

    data = json.loads(output_path.read_text())
    state = data.get("state", "unknown")
    success = bool(data.get("success", False))

    if spec.expect_state is not None:
        # Negative fixture: PASS when the observed state matches the expected
        # failure state (e.g. launch_failed for an unsupported loader/version).
        passed = state == spec.expect_state
    else:
        passed = success and state == "running"

    return ComboResult(
        spec_label=spec.label(),
        passed=passed,
        state=state,
        success=success,
        expected_state=spec.expect_state,
        duration_ms=int(data.get("durationMs", 0)),
        duration_wall_s=wall,
        failure_kind=data.get("failureKind"),
        failure_summary=data.get("failureSummary"),
        launch_log_dir=data.get("launchLogDir"),
        minecraft_log_tail=data.get("minecraftLogTail", []) or [],
        harness_error=harness_error,
    )


# CLI: run a single combo for the checkpoint. -------------------------------

if __name__ == "__main__":
    import argparse

    from workspace import vanilla_modlist

    parser = argparse.ArgumentParser(description="Run one launch-verification combo.")
    parser.add_argument("--loader", default="vanilla")
    parser.add_argument("--mc", default="1.20.4")
    parser.add_argument("--display", default="xvfb", choices=["xvfb", "native"])
    parser.add_argument("--binary", default=None)
    parser.add_argument("--timeout", type=int, default=600)
    parser.add_argument("--success-after", type=int, default=25)
    parser.add_argument("--keep", action="store_true", help="keep the temp workspace")
    args = parser.parse_args()

    modlist_name = "harness-vanilla"
    with HarnessWorkspace(keep=args.keep) as ws:
        print(f"[harness] workspace root: {ws.launcher_root}")
        ws.seed_offline_account()
        ws.create_modlist(vanilla_modlist(modlist_name))
        spec = ComboSpec(
            loader=args.loader,
            mc_version=args.mc,
            modlist_name=modlist_name,
            timeout_seconds=args.timeout,
            success_after_seconds=args.success_after,
        )
        print(f"[harness] running {spec.label()} (display={args.display}) ...")
        result = run_combo(spec, ws, binary=args.binary, display=args.display)
        print(json.dumps(result.to_json(), indent=2))
        raise SystemExit(0 if result.passed else 1)
