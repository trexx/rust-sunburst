// SPDX-License-Identifier: GPL-2.0-or-later

// The typed edge of the management API. Every panel goes through here so the
// token is attached in exactly one place.

const TOKEN_KEY = "sunburst.token";

export function getToken(): string {
  return localStorage.getItem(TOKEN_KEY) ?? "";
}

export function setToken(token: string): void {
  localStorage.setItem(TOKEN_KEY, token);
}

export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
}

/** Thrown for any non-2xx, carrying the server's message rather than a status. */
export class ApiError extends Error {
  constructor(
    readonly status: number,
    message: string,
  ) {
    super(message);
  }

  /** Whether the token is the problem, so the UI can ask for a new one. */
  get isAuth(): boolean {
    return this.status === 401;
  }
}

/** The error for a refused request, with the server's own message. */
async function failure(response: Response): Promise<ApiError> {
  // The server sends {"error": "..."} for everything it refuses. Falling back
  // to the status text matters for the cases it cannot, such as a proxy
  // returning HTML.
  let message = response.statusText;
  try {
    const parsed = await response.json();
    if (parsed && typeof parsed.error === "string") message = parsed.error;
  } catch {
    // Not JSON. The status text is the best available answer.
  }
  return new ApiError(response.status, message);
}

async function request<T>(
  method: string,
  path: string,
  body?: unknown,
): Promise<T> {
  const headers: Record<string, string> = {
    Authorization: `Bearer ${getToken()}`,
  };
  if (body !== undefined) headers["Content-Type"] = "application/json";

  const response = await fetch(path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
  });

  if (!response.ok) throw await failure(response);

  if (response.status === 204 || response.headers.get("content-length") === "0") {
    return undefined as T;
  }
  const text = await response.text();
  return (text ? JSON.parse(text) : undefined) as T;
}

/** Upload raw bytes (an image), returning the parsed JSON answer. */
async function putBlob<T>(path: string, blob: Blob): Promise<T> {
  const response = await fetch(path, {
    method: "PUT",
    headers: { Authorization: `Bearer ${getToken()}`, "Content-Type": blob.type },
    body: blob,
  });
  if (!response.ok) throw await failure(response);
  return (await response.json()) as T;
}

/** Fetch raw bytes, or `null` for a 404. An `<img>` cannot send the bearer
 *  token, so images are fetched here and shown through an object URL. */
async function getBlob(path: string): Promise<Blob | null> {
  const response = await fetch(path, {
    headers: { Authorization: `Bearer ${getToken()}` },
  });
  if (response.status === 404) return null;
  if (!response.ok) throw await failure(response);
  return await response.blob();
}

export const api = {
  get: <T>(path: string) => request<T>("GET", path),
  post: <T>(path: string, body?: unknown) => request<T>("POST", path, body ?? {}),
  put: <T>(path: string, body: unknown) => request<T>("PUT", path, body),
  del: <T>(path: string) => request<T>("DELETE", path),
  putBlob,
  getBlob,
};

// ---------------------------------------------------------------- shapes

export interface EncoderSession {
  pid: number;
  process: string;
  width: number;
  height: number;
}

export interface Status {
  pid: number;
  uptime_secs: number;
  elevated: boolean;
  version: string;
  /** Other processes' NVENC sessions; null when NVML could not be asked. */
  other_encoders: EncoderSession[] | null;
  /** null when the host could not read it — not the same as "off". */
  autostart: boolean | null;
  running_app: RunningApp | null;
  paired_clients: number;
  sessions: number;
  lan_exposed: boolean;
}

export interface RunningApp {
  app_id: number;
  pid: number;
  started_at: number;
  /** How the server follows it: its job, a named process, or not at all (a URI). */
  tracking: "job" | "process" | "untracked";
  /** Launched, but the named process has not appeared yet. */
  starting: boolean;
}

export interface Quirks {
  ref_invalidation: boolean;
  intra_refresh: boolean;
  slice_output: boolean;
  needs_annexb_startcodes: boolean;
  max_bitrate_hint: number;
}

export interface Client {
  id: number;
  name: string;
  model: string;
  abi: string;
  quirks: Quirks;
  paired_at: number;
  last_seen: number | null;
}

export interface PendingPair {
  id: number;
  name: string;
  model: string;
  abi: string;
  received_at: number;
  /** True until the client has sent its confirmation; a PIN cannot work yet. */
  awaiting_client: boolean;
}

export interface Session {
  id: number;
  client_id: number;
  client_name: string;
  codec: string;
  width: number;
  height: number;
  fps: number;
  bitrate_kbps: number;
  started_at: number;
  app_id: number | null;
}

export interface PrepCommand {
  run: string;
  undo: string | null;
}

export interface Overrides {
  bitrate_kbps: number | null;
  codec: string | null;
  width: number | null;
  height: number | null;
  fps: number | null;
  preset: number | null;
}

export interface AppEntry {
  id: number;
  name: string;
  exe: string;
  args: string[];
  working_dir: string | null;
  prep: PrepCommand[];
  overrides: Overrides;
  /** The game's own executable, for an entry that hands off to it. */
  wait_process: string | null;
}

/** An app's box art, as the server stores it and the TV caches it. */
export interface ArtInfo {
  app_id: number;
  /** Hex of the content digest the TV caches by. */
  digest: string;
  len: number;
  format: string;
}

/** The server's limit on a stored image. */
export const ART_MAX_BYTES = 512 * 1024;

export type Codec = "auto" | "hevc" | "av1" | "h264";
export type RateControl = "cbr" | "vbr";
export type CaptureBackend = "auto" | "wgc" | "dda" | "nvfbc";

export interface StreamSettings {
  port: number;
  bitrate_kbps: number;
  codec: Codec;
  audio: boolean;
  audio_device: string | null;
  mic_device: string | null;
  audio_bitrate_kbps: number;
  match_resolution: boolean;
  virtual_display: boolean;
  capture_output: number | null;
  // Advanced video
  hdr: boolean;
  preset: number;
  rate_control: RateControl;
  slices: number;
  idr_period: number;
  dpb_depth: number;
  capture_backend: CaptureBackend;
  min_bitrate_kbps: number;
  max_bitrate_kbps: number;
  fps_cap: number;
  // Advanced audio
  audio_frame_us: number;
  audio_fec: boolean;
  audio_complexity: number;
}

export interface InputSettings {
  mouse_sensitivity: number;
  gamepad_deadzone: number;
  disable_epp: boolean;
}

export interface Settings {
  web: {
    bind: string;
    port: number;
    token: string;
    assets_dir: string;
  };
  stream: StreamSettings;
  input: InputSettings;
}

export interface AudioDevices {
  devices: string[];
}

export interface StageMetrics {
  stage: string;
  count: number;
  p50_ns: number;
  p95_ns: number;
  p99_ns: number;
  max_ns: number;
}

export interface Metrics {
  stages: StageMetrics[];
  dropped_samples: number;
  dropped_by_thread: [string, number][];
  unregistered_threads: number;
  /** When true these percentiles were computed over a thinned sample. */
  lossy: boolean;
}

// ---------------------------------------------------------------- formatting

export function formatNs(ns: number): string {
  if (ns >= 1_000_000) return `${(ns / 1_000_000).toFixed(2)} ms`;
  if (ns >= 1_000) return `${(ns / 1_000).toFixed(1)} µs`;
  return `${ns} ns`;
}

export function formatDuration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`;
  const m = Math.floor(seconds / 60);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  return `${Math.floor(h / 24)}d ${h % 24}h`;
}

export function formatDate(unixSeconds: number): string {
  return new Date(unixSeconds * 1000).toLocaleString();
}
