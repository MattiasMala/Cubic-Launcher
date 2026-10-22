type LogLevel = "trace" | "debug" | "info" | "warn" | "error";

// Frontend log lines go to the webview console only. The in-memory buffer that
// fed an on-screen debug panel went away with the panel, which was never
// mounted (audit 101, 085 scheda 12).
const consoleMethods: Record<LogLevel, (...args: unknown[]) => void> = {
  trace: console.debug.bind(console),
  debug: console.debug.bind(console),
  info:  console.info.bind(console),
  warn:  console.warn.bind(console),
  error: console.error.bind(console),
};

function addEntry(level: LogLevel, tag: string, msg: string, data?: unknown): void {
  consoleMethods[level](`[${tag}] ${msg}`, data ?? "");
}

export const logger = {
  trace: (tag: string, msg: string, data?: unknown) => addEntry("trace", tag, msg, data),
  debug: (tag: string, msg: string, data?: unknown) => addEntry("debug", tag, msg, data),
  info:  (tag: string, msg: string, data?: unknown) => addEntry("info",  tag, msg, data),
  warn:  (tag: string, msg: string, data?: unknown) => addEntry("warn",  tag, msg, data),
  error: (tag: string, msg: string, data?: unknown) => addEntry("error", tag, msg, data),
};
