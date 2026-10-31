// Small presentational pieces shared by both pages, plus the vocabulary that
// turns the server's machine words into labels.
//
// Nothing here fetches. The two pages own their own data because they need
// different slices of it: the console polls `/status` and `/control` and never
// touches a recording; the recordings page loads a day and never asks for the
// unit state.

import type { ControlAction, ControlTarget, Status, Tone } from './api'

export function CardHead({
  title,
  state,
  meta,
}: {
  title: string
  state: { text: string; tone: Tone }
  /** Right-aligned monospace detail: a unit name, an address. Not a metric. */
  meta?: { text: string; title: string }
}) {
  return (
    <div className="card-head">
      <h2>{title}</h2>
      <span className={`chip ${toneClass(state.tone)}`}>
        <i /> {state.text}
      </span>
      {meta && (
        <span className="card-unit" title={meta.title}>
          {meta.text}
        </span>
      )}
    </div>
  )
}

export function PanelHead({ title, meta }: { title: string; meta: string }) {
  return (
    <div className="panel-head">
      <h2>{title}</h2>
      <span>{meta}</span>
    </div>
  )
}

export function Empty({ text }: { text: string }) {
  return <p className="empty">{text}</p>
}

export function toneClass(tone: Tone): string {
  switch (tone) {
    case 'ok':
      return 'ok'
    case 'warn':
      return 'warn'
    case 'bad':
      return 'bad'
    default:
      return 'muted'
  }
}

export function targetLabel(target: ControlTarget): string {
  return target === 'server' ? '服务端' : '采集端'
}

export function actionLabel(action: ControlAction): string {
  switch (action) {
    case 'start':
      return '启动'
    case 'stop':
      return '停止'
    default:
      return '重启'
  }
}

export function count(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return '—'
  return value.toLocaleString()
}

export function timeQualityLabel(quality: string | undefined): string {
  switch (quality) {
    case 'synced':
      return '已同步'
    case 'corrected':
      return '已校正'
    case 'unsynced':
      return '未同步'
    default:
      return '未知'
  }
}

/** Renders an argv the way a shell would show it, quoting what needs it. */
export function shellLine(argv: string[]): string {
  if (argv.length === 0) return '—'
  return argv
    .map((part) => (/[\s"'$`]/.test(part) ? `'${part.replaceAll("'", "'\\''")}'` : part))
    .join(' ')
}

export function asArgv(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : []
}

export function asText(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null
}

export function serviceLabel(status: Status): string {
  if (status.service.state === 'degraded') return '服务异常'
  return status.signal_state === 'silent' ? '静默录制' : '服务正常'
}

export function serviceTone(status: Status): Tone {
  if (status.service.state === 'degraded' || status.signal_state === 'silent') return 'warn'
  return status.signal_state === 'ok' ? 'ok' : 'idle'
}
