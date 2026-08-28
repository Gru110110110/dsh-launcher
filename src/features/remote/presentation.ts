import type { RemoteTunnelState } from "@/platform/generated/bindings";

/**
 * Builds an <img>-safe data URL for the SVG QR code returned by the backend.
 */
export function qrDataUrl(svg: string): string {
  return `data:image/svg+xml;charset=utf-8,${encodeURIComponent(svg)}`;
}

/**
 * Groups a connection password into chunks of four characters for on-screen
 * readability. The raw password (from the snapshot) remains the value used
 * for actual connections; this is display-only.
 */
export function formatRemotePassword(password: string): string {
  const trimmed = password.trim();
  if (trimmed.length <= 4) return trimmed;
  const groups: string[] = [];
  for (let index = 0; index < trimmed.length; index += 4) {
    groups.push(trimmed.slice(index, index + 4));
  }
  return groups.join(" ");
}

type PublicStateCopy = {
  key: string;
  tone: "info" | "error";
};

/**
 * Maps the public tunnel state to the status copy shown under the public
 * toggle. "running" and "off" render no status line.
 */
export function publicStateCopy(
  state: RemoteTunnelState,
): PublicStateCopy | null {
  switch (state) {
    case "starting":
      return { key: "remote.publicStarting", tone: "info" };
    case "failed":
      return { key: "remote.publicFailed", tone: "error" };
    case "off":
    case "running":
      return null;
  }
}

/**
 * Eight ASCII digits — the same contract the backend enforces for both
 * generated and user-chosen connection passwords.
 */
export function isValidRemotePassword(password: string): boolean {
  return /^\d{8}$/.test(password);
}

/**
 * Narrows an unknown thrown value to an AppError-like record with the
 * expected code (e.g. "remoteDisclaimerRequired").
 */
export function isErrorCode(error: unknown, code: string): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    (error as { code?: unknown }).code === code
  );
}
