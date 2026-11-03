// Typed access to the server API.
//
// Every field below mirrors a struct in crates/server/src/http_server.rs. When
// one of them gains a field, add it here too: the page renders `unknown`/`—`
// for anything missing rather than filling the gap with a plausible number,
// which is the same rule the server follows for `null`.

export interface Status {
  service: {
    state: string
    version: string
    now_utc: string
    last_error: string | null
    /** How this process was started; null when it did not record it. */
    launch: {
      argv: string[]
      config_path: string | null
      pid: number
    } | null
  }
  recording: { state: string }
  microphone: {
    state: string
    device: string
    /** Format of the recording: always 16 kHz mono. */
    capture_rate_hz: number
    channels: number
    sample_format: string
    /** What the device itself runs at; 0 until negotiated. */
    device_rate_hz: number
    device_channels: number
  }
  detector: {
    state: string
    noise_floor_dbfs: number | null
    band_noise_floor_dbfs: number | null
    events_total: number
    events_discarded_short: number
    event_fk_retries: number
  }
  storage: {
    /** Bytes in the recordings table; null when the database could not answer. */
    bytes: number | null
    max_bytes: number
    cleanup_target_bytes: number
    reserve_bytes: number
    free_bytes: number | null
    recording_dir: string
    storage_pressure: boolean
    /** Rows the byte total is made of. */
    recordings: number | null
  }
  capture: {
    /** Peak of the newest block, in dBFS; null before the first block. */
    level_peak_dbfs: number | null
    /** RMS of the newest block, in dBFS; null before the first block. */
    level_rms_dbfs: number | null
    xrun_count: number
    lost_frames_total: number
    capture_ring_overflow_frames: number
    dropped_detection_blocks: number
    gap_detection_limited: boolean
    clipped_samples_total: number
  }
  source: {
    kind: string
    node_connected: boolean
    /** `ip:port` the newest datagram came from; null before the first one. */
    peer: string | null
    /** Wall clock of the newest datagram; null before the first one. */
    last_packet_at_utc: string | null
    packets_received: number
    packets_lost: number
    packets_duplicate: number
    packets_reordered: number
    protocol_errors: number
    /** The node's own report: argv, config path, version. Untyped on purpose —
     *  it is whatever that node sent, and guessing fields would be invention. */
    node_report: Record<string, unknown> | null
    node_report_at_utc: string | null
    /** Frames the node said it never sent (uplink gating); 0 = nothing gated. */
    gated_frames: number
    gate_announcements: number
  }
  queues: {
    recording_high_water: number
    detection_high_water: number
    recording_control_slots: number
    detection_control_slots: number
  }
  dsp: { count: number; avg_ns: number; p99_ns: number; max_ns: number }
  time: {
    time_quality: string
    time_synced: boolean
    clock_drift_ppm: number | null
  }
  stt: {
    enabled: boolean
    /** "stream" | "segment" */
    mode: string
    /** "off" | "starting" | "ready" | "segment" | "error" */
    state: string
    connected: boolean
    engine: string
    finals_total: number
    dropped_blocks: number
    /** The phrase being recognised right now; never stored. */
    partial: string | null
    error: string | null
    /** The recogniser command line the worker uses; null when it is off. */
    launch: { command: string; args: string[] } | null
  }
  /**
   * `ok` | `silent` | `dead` | `unknown`.
   *
   * `silent` is digital silence (every sample zero). `dead` is the one that
   * matters: samples arrive and carry only dither, so a muted capsule or an
   * unseated microphone used to be indistinguishable from a working one.
   */
  signal_state: string
}

/** One service unit the control centre can start, stop or restart. */
export interface UnitView {
  /** "server" | "node" */
  target: string
  unit: string
  /** active | activating | deactivating | inactive | failed | reloading | unknown */
  state: string
}

export interface ControlView {
  enabled: boolean
  available: boolean
  /** Why control cannot act, straight from the server's own probe. */
  reason: string | null
  systemctl: string
  units: UnitView[]
}

export interface ControlOutcome {
  ok: boolean
  /** systemctl's own complaint, or null on success. */
  message: string | null
  control: ControlView
}

export type ControlTarget = 'server' | 'node'
export type ControlAction = 'start' | 'stop' | 'restart'

/** One recognised phrase from the streaming recogniser. */
export interface Utterance {
  id: string
  started_at_utc: string
  ended_at_utc: string
  engine: string
  language: string | null
  text: string
}

/** One written WAV file: a stretch of audio the server has on disk. */
export interface Recording {
  id: string
  started_at_utc: string
  ended_at_utc: string
  capture_rate_hz: number
  channels: number
  frame_count: number
  size_bytes: number
  duration_ms: number
  lost_frames: number
  audio_url: string
}

/**
 * One snore the detector or the classifier reported. It is not a diagnosis: the
 * labels below only record what the reviewer decided.
 *
 * Two annotators write these rows and the page must not mix them up: the
 * server's own `rule-v1` (band energy above the noise floor — it fires on
 * speech, a TV and a bed-time story just as readily as on snoring) and the
 * sidecar's `apple-soundanalysis-v1` (a classifier that returns the label
 * `snoring` with a confidence). `source` carries the name in both cases, which
 * is how a row can be told apart.
 */
export interface Episode {
  id: string
  /** The recording that holds this episode's audio; null when none is known. */
  recording_id: string | null
  started_at_utc: string
  ended_at_utc: string
  duration_ms: number
  /** `rule-v1` or `apple-soundanalysis-v1`. */
  source: string
  label: string | null
  confidence: number | null
  review_status: 'snore' | 'not_snore' | 'unreviewed'
}

export interface Day {
  date: string
  recording_count: number
  episode_count: number
  recorded_ms: number
  lost_frames: number
}

async function get<T>(path: string): Promise<T> {
  const response = await fetch(path)
  if (!response.ok) {
    const body = await response.text()
    throw new Error(`${response.status} ${body.slice(0, 200)}`)
  }
  return (await response.json()) as T
}

export const api = {
  status: () => get<Status>('/api/v1/status'),

  control: () => get<ControlView>('/api/v1/control'),

  /**
   * Runs one start/stop/restart. A non-2xx answer is a *result*, not a thrown
   * error: the body carries systemctl's reason and the unit states read back
   * afterwards.
   */
  async controlAction(target: ControlTarget, action: ControlAction): Promise<ControlOutcome> {
    const response = await fetch('/api/v1/control', {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ target, action }),
    })
    const text = await response.text()
    if (!text) {
      // 204 or a truncated body: nothing was claimed, so claim nothing.
      throw new Error(`${response.status} 空响应`)
    }
    return JSON.parse(text) as ControlOutcome
  },

  async days(from: string, to: string): Promise<Day[]> {
    const data = await get<{ days: Day[] }>(`/api/v1/days?from=${from}&to=${to}`)
    return data.days
  },

  async recordings(date: string): Promise<Recording[]> {
    const data = await get<{ recordings: Recording[] }>(`/api/v1/recordings?date=${date}`)
    return data.recordings
  },

  async events(date: string): Promise<Episode[]> {
    const data = await get<{ events: Episode[] }>(`/api/v1/events?date=${date}`)
    return data.events
  },

  async utterances(date: string): Promise<Utterance[]> {
    const data = await get<{ utterances: Utterance[] }>(`/api/v1/utterances?date=${date}`)
    return data.utterances
  },

  /**
   * Sets a candidate's review label. The server still exposes this and the
   * column still holds labels from earlier nights, but the Portal no longer
   * offers the controls: the page reports candidates, it does not grade them.
   * Kept typed so the endpoint stays documented on this side too.
   */
  async review(
    eventId: string,
    status: 'snore' | 'not_snore' | 'unreviewed',
  ): Promise<void> {
    const response = await fetch(`/api/v1/events/${eventId}`, {
      method: 'PATCH',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ review_status: status }),
    })
    if (!response.ok) throw new Error(await response.text())
  },
}

// --- formatting helpers ----------------------------------------------------

export function localTime(iso: string): string {
  return new Date(iso).toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  })
}

export function localClock(iso: string): string {
  return new Date(iso).toLocaleTimeString('zh-CN', {
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  })
}

/**
 * "how long ago" for a heartbeat. The point of showing it next to 在线 is that a
 * node which stopped 20 seconds ago and one that stopped an hour ago are
 * different problems.
 */
export function ageText(iso: string | null, now: number = Date.now()): string {
  if (!iso) return '—'
  const delta = now - Date.parse(iso)
  if (!Number.isFinite(delta)) return '—'
  const seconds = Math.round(delta / 1000)
  if (seconds <= 1) return '刚刚'
  if (seconds < 60) return `${seconds} 秒前`
  const minutes = Math.round(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.round(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  return `${Math.round(hours / 24)} 天前`
}

export function localDateTime(iso: string | null): string {
  if (!iso) return '—'
  return new Date(iso).toLocaleString('zh-CN', {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  })
}

export function hoursMinutes(ms: number): string {
  const totalMinutes = Math.round(ms / 60000)
  const hours = Math.floor(totalMinutes / 60)
  const minutes = totalMinutes % 60
  if (hours === 0) return `${minutes} 分钟`
  return `${hours} 小时 ${minutes} 分`
}

/**
 * Duration with the unit that actually fits. `hoursMinutes` rounds a 30-second
 * segment to "0 分钟", which reads as "nothing was recorded".
 */
export function duration(ms: number): string {
  if (!Number.isFinite(ms) || ms <= 0) return '0 秒'
  // A recognised phrase can be 200 ms ("需要"). Rounding that to "0 秒" reads
  // like nothing was captured, so sub-second lengths keep one decimal.
  if (ms < 1_000) return `${(ms / 1000).toFixed(1)} 秒`
  if (ms < 60_000) return `${Math.round(ms / 1000)} 秒`
  return hoursMinutes(ms)
}

export function bytes(value: number | null | undefined): string {
  // A missing field used to take the whole page down, because a formatter that
  // throws inside render unmounts the tree. Every formatter here is total.
  if (value == null || !Number.isFinite(value)) return '—'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let scaled = value
  let unit = 0
  while (scaled >= 1024 && unit < units.length - 1) {
    scaled /= 1024
    unit += 1
  }
  return `${scaled.toFixed(unit === 0 ? 0 : 1)} ${units[unit]}`
}

/** Fraction of `total` that `value` occupies, clamped and total. */
export function share(value: number | null | undefined, total: number): number {
  if (value == null || !Number.isFinite(value) || total <= 0) return 0
  return Math.min(Math.max(value / total, 0), 1)
}

export function todayISO(): string {
  const now = new Date()
  const offset = now.getTimezoneOffset() * 60000
  return new Date(now.getTime() - offset).toISOString().slice(0, 10)
}

export function shiftDate(date: string, days: number): string {
  const parsed = new Date(`${date}T12:00:00`)
  parsed.setDate(parsed.getDate() + days)
  const offset = parsed.getTimezoneOffset() * 60000
  return new Date(parsed.getTime() - offset).toISOString().slice(0, 10)
}

// --- state vocabularies ----------------------------------------------------

/**
 * Unit states systemd reports. `available` separates "the unit is unknown" from
 * "nothing could be asked at all" (no systemd, control disabled, node on another
 * host) — those are different facts and the card must not blur them.
 */
export function unitStateLabel(state: string, available: boolean): { text: string; tone: Tone } {
  if (!available) return { text: '不可查', tone: 'idle' }
  switch (state) {
    case 'active':
      return { text: '运行中', tone: 'ok' }
    case 'activating':
      return { text: '启动中', tone: 'warn' }
    case 'deactivating':
      return { text: '停止中', tone: 'warn' }
    case 'inactive':
      return { text: '已停止', tone: 'idle' }
    case 'failed':
      return { text: '已失败', tone: 'bad' }
    case 'reloading':
      return { text: '重载中', tone: 'warn' }
    default:
      // `unknown` means nothing could be asked — a split deployment, a missing
      // systemd, or a unit that is not installed here.
      return { text: '未知', tone: 'idle' }
  }
}

export type Tone = 'ok' | 'warn' | 'bad' | 'idle'

export function sttStateLabel(status: Status['stt']): { text: string; tone: Tone } {
  switch (status.state) {
    case 'ready':
      return { text: '识别中', tone: 'ok' }
    case 'starting':
      return { text: '等待识别器', tone: 'warn' }
    case 'segment':
      return { text: '逐段转写', tone: 'ok' }
    case 'error':
      return { text: '识别器故障', tone: 'bad' }
    default:
      return { text: '未开启', tone: 'idle' }
  }
}
