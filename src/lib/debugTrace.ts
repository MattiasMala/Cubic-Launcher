import { invoke } from "@tauri-apps/api/core";

const isTauri = () => "__TAURI_INTERNALS__" in window;

// The trace is a development tool (shell-10): a production build does not
// even make the call, and the backend writes nothing in release either.
export function appendDebugTrace(scope: string, payload: unknown) {
  if (!import.meta.env.DEV || !isTauri()) return;

  const detail =
    typeof payload === "string"
      ? payload
      : JSON.stringify(payload, null, 0);

  void invoke("append_debug_trace_command", {
    entry: `[${scope}] ${detail}`,
  }).catch(() => {
    // Intentionally ignore debug trace write failures.
  });
}
