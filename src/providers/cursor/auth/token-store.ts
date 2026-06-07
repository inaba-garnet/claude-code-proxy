import { readFile, unlink } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";
import { keychainGet, keychainDelete } from "../../../keychain.ts";
import { parseJwtClaims, tokenExpiryMs } from "./jwt.ts";

export interface CursorAuth {
  accessToken: string;
  refreshToken?: string;
  apiKey?: string;
  expires?: number;
  userId?: string;
  email?: string;
  source: string;
}

interface CursorAuthFile {
  accessToken?: string;
  refreshToken?: string;
  apiKey?: string;
}

const CURSOR_KEYCHAIN_ACCOUNT = "cursor-user";
const ACCESS_TOKEN_SERVICE = "cursor-access-token";
const REFRESH_TOKEN_SERVICE = "cursor-refresh-token";
const API_KEY_SERVICE = "cursor-api-key";

export async function loadCursorAuth(env: NodeJS.ProcessEnv = process.env): Promise<CursorAuth | undefined> {
  const envToken = env.CCP_CURSOR_AUTH_TOKEN || env.CURSOR_AUTH_TOKEN;
  if (envToken) return authFromToken(envToken, "environment");

  if (process.platform === "darwin") {
    const accessToken = keychainGet(ACCESS_TOKEN_SERVICE, CURSOR_KEYCHAIN_ACCOUNT);
    if (accessToken) {
      return enrich({
        accessToken,
        refreshToken: keychainGet(REFRESH_TOKEN_SERVICE, CURSOR_KEYCHAIN_ACCOUNT),
        apiKey: keychainGet(API_KEY_SERVICE, CURSOR_KEYCHAIN_ACCOUNT),
        source: "macOS Keychain",
      });
    }
  }

  for (const path of cursorAuthFileCandidates(env)) {
    try {
      const raw = await readFile(path, "utf8");
      const parsed = JSON.parse(raw) as CursorAuthFile;
      if (parsed.accessToken) return enrich({ ...parsed, accessToken: parsed.accessToken, source: path });
    } catch (err: any) {
      if (err?.code !== "ENOENT") throw err;
    }
  }

  return undefined;
}

export async function clearCursorAuth(): Promise<void> {
  if (process.platform === "darwin") {
    keychainDelete(ACCESS_TOKEN_SERVICE, CURSOR_KEYCHAIN_ACCOUNT);
    keychainDelete(REFRESH_TOKEN_SERVICE, CURSOR_KEYCHAIN_ACCOUNT);
    keychainDelete(API_KEY_SERVICE, CURSOR_KEYCHAIN_ACCOUNT);
    return;
  }

  for (const path of cursorAuthFileCandidates(process.env)) {
    try {
      await unlink(path);
    } catch (err: any) {
      if (err?.code !== "ENOENT") throw err;
    }
  }
}

export function cursorAuthLocation(): string {
  return process.platform === "darwin" ? "Cursor macOS Keychain" : cursorAuthFileCandidates(process.env)[0]!;
}

export function missingAuthMessage(): string {
  return [
    "Cursor authentication was not found.",
    "Run `cursor-agent login` once, or set CCP_CURSOR_AUTH_TOKEN/CURSOR_AUTH_TOKEN.",
    "On macOS the provider reads Keychain services cursor-access-token, cursor-refresh-token, and cursor-api-key for account cursor-user.",
  ].join(" ");
}

export function expiredAuthMessage(auth: CursorAuth): string {
  const expires = auth.expires ? new Date(auth.expires).toISOString() : "unknown";
  return `Cursor access token from ${auth.source} is expired or near expiry (${expires}). Run \`cursor-agent login\` again or set CCP_CURSOR_AUTH_TOKEN.`;
}

function authFromToken(accessToken: string, source: string): CursorAuth {
  return enrich({ accessToken, source });
}

function enrich(auth: Omit<CursorAuth, "expires" | "userId" | "email"> & Partial<CursorAuth>): CursorAuth {
  const claims = parseJwtClaims(auth.accessToken);
  return {
    ...auth,
    expires: tokenExpiryMs(auth.accessToken),
    userId: typeof claims?.sub === "string" ? claims.sub : auth.userId,
    email: typeof claims?.email === "string" ? claims.email : auth.email,
  };
}

function cursorAuthFileCandidates(env: NodeJS.ProcessEnv): string[] {
  if (process.platform === "win32") {
    const appData = env.APPDATA || join(homedir(), "AppData", "Roaming");
    return [join(appData, "Cursor", "auth.json")];
  }
  if (process.platform === "darwin") {
    return [join(homedir(), ".cursor", "auth.json")];
  }
  const configHome = env.XDG_CONFIG_HOME || join(homedir(), ".config");
  return [join(configHome, "cursor", "auth.json")];
}
