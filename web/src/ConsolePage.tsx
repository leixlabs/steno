// The console page: "is it running, is anything reaching the disk, can I stop it".
//
// This page owns the operational state — `/status` and `/control` — and nothing
// about a night. The recordings page owns the day. They are separate documents
// because they are read for different reasons: one at 3am to check the pipeline,
// one in the morning to listen. Loading them together meant the console also
// waited on a day's worth of segments.

import { useCallback, useEffect, useState } from 'react'
import {
  ageText,
  api,
  bytes,
  duration,
  share,
  sttStateLabel,
  unitStateLabel,
  type ControlAction,
  type ControlTarget,
  type ControlView,
  type Status,
  type Tone,
} from './api'
import {
  CardHead,
  PanelHead,
  actionLabel,
  asArgv,
  asText,
  count,
  shellLine,
  targetLabel,
  timeQualityLabel,
} from './ui'

export default function ConsolePage() {
  const [status, setStatus] = useState<Status | null>(null)
  // Distinguishes "the first poll has not answered yet" from "we lost it", so
  // the service card does not claim 不可达 for the first four seconds.
  const [everLoaded, setEverLoaded] = useState(false)
  const [control, setControl] = useState<ControlView | null>(null)
  const [notice, setNotice] = useState<{ tone: Tone; text: string } | null>(null)
  const [busy, setBusy] = useState<string | null>(null)

  const refreshStatus = useCallback(async () => {
    try {
      setStatus(await api.status())
      setEverLoaded(true)
    } catch {
      // A status blip must not blank the page; the next poll retries.
      setStatus(null)
    }
  }, [])

  const refreshControl = useCallback(async () => {
    try {
      setControl(await api.control())
    } catch {
      setControl(null)
    }
  }, [])

  useEffect(() => {
    void refreshStatus()
    void refreshControl()
    const statusTimer = setInterval(refreshStatus, 4000)
    // Slower than the status poll: every answer spawns `systemctl is-active`
    // twice on the server, and a unit state does not change every few seconds.
    const controlTimer = setInterval(refreshControl, 10000)
    return () => {
      clearInterval(statusTimer)
      clearInterval(controlTimer)
    }
  }, [refreshStatus, refreshControl])

  /** Runs one unit action and reports what actually came back. */
  const act = async (target: ControlTarget, action: ControlAction) => {
    setBusy(`${target}:${action}`)
    setNotice(null)
    try {
      const outcome = await api.controlAction(target, action)
      setControl(outcome.control)
      setNotice(
        outcome.ok
          ? { tone: 'ok', text: `${targetLabel(target)}：已执行 ${actionLabel(action)}` }
          : { tone: 'bad', text: outcome.message ?? '动作被拒绝，但没有返回原因' },
      )
    } catch (cause) {
      // Stopping or restarting the server unit takes this page's own backend
      // down, so a failed request is expected there and is not a failure. The
      // next poll decides whether it worked.
      if (target === 'server' && action !== 'start') {
        setNotice({
          tone: 'warn',
          text: `已下发${actionLabel(action)}；服务端正在${
            action === 'restart' ? '重启' : '停止'
          }，页面会自动重试。`,
        })
      } else {
        setNotice({ tone: 'bad', text: (cause as Error).message })
      }
    } finally {
      setBusy(null)
      // Read the truth back rather than assuming the action landed.
      void refreshStatus()
      void refreshControl()
    }
  }

  return (
    <>
      {notice && <div className={`banner ${notice.tone}`}>{notice.text}</div>}

      <section className="ops">
        <ServiceCard
          status={status}
          control={control}
          busy={busy}
          act={act}
          everLoaded={everLoaded}
        />
        <CaptureCard status={status} control={control} busy={busy} act={act} />
        <TranscriptionCard status={status} />
      </section>

      <LaunchPanel status={status} />

      {/* Last, not first: the pool is a slow-moving fact — it answers "how many
          nights will fit" once a week, while the three cards above answer "is it
          running" every time the page is opened. */}
      <StoragePoolCard status={status} />
    </>
  )
}

// --- operational cards ------------------------------------------------------

function ServiceCard({
  status,
  control,
  busy,
  act,
  everLoaded,
}: {
  status: Status | null
  control: ControlView | null
  busy: string | null
  act: (target: ControlTarget, action: ControlAction) => void
  everLoaded: boolean
}) {
  return (
    <article className="card">
      <CardHead title="服务端" state={serverState(status, everLoaded)} />
      <dl className="facts tight">
        <div>
          <dt>录制</dt>
          <dd>{recordingText(status)}</dd>
        </div>
        <div>
          <dt>信号</dt>
          <dd className={status?.signal_state === 'dead' ? 'bad' : undefined}>
            {signalText(status)}
          </dd>
        </div>
        <div>
          <dt>电平</dt>
          <dd>
            {status?.capture.level_peak_dbfs == null
              ? '—'
              : `${status.capture.level_peak_dbfs.toFixed(1)} dBFS 峰值`}
          </dd>
        </div>
        <div>
          <dt>时间质量</dt>
          <dd>{timeQualityLabel(status?.time.time_quality)}</dd>
        </div>
        <div>
          <dt>时钟漂移</dt>
          <dd>
            {status?.time.clock_drift_ppm == null
              ? '—'
              : `${status.time.clock_drift_ppm.toFixed(1)} ppm`}
          </dd>
        </div>
      </dl>
      {signalHint(status) && (
        <p className="signal-hint">{signalHint(status)}</p>
      )}
      <UnitActions target="server" control={control} busy={busy} act={act} />
    </article>
  )
}

function CaptureCard({
  status,
  control,
  busy,
  act,
}: {
  status: Status | null
  control: ControlView | null
  busy: string | null
  act: (target: ControlTarget, action: ControlAction) => void
}) {
  const isNode = status?.source.kind === 'net'
  const peer = status?.source.peer ?? null
  return (
    <article className="card">
      <CardHead
        title={isNode ? '采集端（节点）' : '采集端（本机）'}
        state={isNode ? nodeState(status) : { text: micLabel(status), tone: micTone(status) }}
        // The node's address is the one fact the wire protocol does not carry:
        // it comes from the receiving socket, which is why this is the only
        // place on the page that can answer "which machine is sending".
        meta={
          isNode && peer
            ? { text: peer, title: '采集节点最后收包的来源地址（IP:端口）' }
            : undefined
        }
      />
      {isNode ? (
        <dl className="facts tight">
          <div>
            <dt>最后收包</dt>
            <dd title={status?.source.last_packet_at_utc ?? ''}>
              {ageText(status?.source.last_packet_at_utc ?? null)}
            </dd>
          </div>
          <div>
            <dt>已收包</dt>
            <dd>{count(status?.source.packets_received)}</dd>
          </div>
          <div>
            <dt>丢包</dt>
            <dd>{count(status?.source.packets_lost)}</dd>
          </div>
          <div>
            <dt>协议错误</dt>
            <dd>{count(status?.source.protocol_errors)}</dd>
          </div>
          {(status?.source.gated_frames ?? 0) > 0 && (
            <div>
              <dt>端侧剔除</dt>
              <dd>{gatedText(status)}</dd>
            </div>
          )}
        </dl>
      ) : (
        <dl className="facts tight">
          <div>
            <dt>设备</dt>
            <dd className="ellipsis" title={status?.microphone.device ?? ''}>
              {status?.microphone.device || '—'}
            </dd>
          </div>
          <div>
            <dt>采样</dt>
            <dd title={formatTitle(status)}>{formatSummary(status)}</dd>
          </div>
          <div>
            <dt>丢帧</dt>
            <dd>{count(status?.capture.lost_frames_total)}</dd>
          </div>
          <div>
            <dt>xrun</dt>
            <dd>{count(status?.capture.xrun_count)}</dd>
          </div>
        </dl>
      )}
      {isNode ? (
        <UnitActions target="node" control={control} busy={busy} act={act} />
      ) : (
        <p className="card-note">
          本机麦克风由服务端进程持有，随服务端启停；节点单元不参与这条链路。
        </p>
      )}
    </article>
  )
}

function TranscriptionCard({ status }: { status: Status | null }) {
  const stt = status?.stt
  const label = stt ? sttStateLabel(stt) : { text: '未知', tone: 'idle' as Tone }
  return (
    <article className="card">
      <CardHead title="语音转写" state={label} />
      <dl className="facts tight">
        <div>
          <dt>引擎</dt>
          <dd>{stt?.engine || '—'}</dd>
        </div>
        <div>
          <dt>模式</dt>
          <dd>{stt ? (stt.mode === 'segment' ? '逐段' : '流式') : '—'}</dd>
        </div>
        <div>
          <dt>已存语句</dt>
          <dd>{count(stt?.finals_total)}</dd>
        </div>
        <div>
          <dt>丢弃块</dt>
          <dd>{count(stt?.dropped_blocks)}</dd>
        </div>
      </dl>
      {stt?.partial && <p className="card-note live">正在识别：{stt.partial}</p>}
      {stt && !stt.enabled && (
        <p className="card-note">
          <code>[stt] enabled = false</code>，录音照常，只是不产生文字。
        </p>
      )}
      {stt?.error && <p className="card-note bad">{stt.error}</p>}
      {stt?.recognizers?.map((r) => (
        <p key={r.name} className={r.missing_lanes.length ? 'card-note bad' : 'card-note'}>
          识别器 {r.name}：{r.connected ? '已连接' : '未连接'}
          {r.connected && ` · 通道 ${r.lanes.join('/') || '—'}`}
          {r.missing_lanes.length > 0 && ` · 缺少通道 ${r.missing_lanes.join('/')}（该进程不提供，不会有结果）`}
          {r.wanted.includes('snore') && ` · 鼾声 ${count(r.snore_events_total)}`}
        </p>
      ))}
      {stt?.enabled && stt.recognizers && !stt.recognizers.some((r) => r.wanted.includes('snore')) && (
        <p className="card-note">没有识别器被要求提供鼾声通道；只有规则检测器 rule-v1 在标注鼾声。</p>
      )}
    </article>
  )
}

function StoragePoolCard({ status }: { status: Status | null }) {
  const storage = status?.storage
  const used = storage?.bytes ?? null
  const max = storage?.max_bytes ?? 0
  const usedShare = share(used, max)
  const targetShare = share(storage?.cleanup_target_bytes, max)
  const free = storage?.free_bytes ?? null
  return (
    <article className="card pool">
      <div className="panel-head">
        <h2>存储池</h2>
        <span>
          {storage ? `${bytes(used)} / ${bytes(max)} · 剩余磁盘 ${bytes(free)}` : '读取中…'}
        </span>
      </div>
      <div className="meter" role="img" aria-label={`已用 ${Math.round(usedShare * 100)}%`}>
        <span
          className={`meter-fill ${storage?.storage_pressure ? 'pressure' : ''}`}
          // A real but tiny pool would otherwise render as an empty bar.
          style={{ width: `${usedShare * 100}%`, minWidth: used ? 3 : 0 }}
        />
        {/* Retention deletes down to this mark, so the operator can see how much
            of the ceiling is actually in use versus reserved for the cleanup. */}
        <span className="meter-mark" style={{ left: `${targetShare * 100}%` }} title="清理目标" />
      </div>
      <dl className="facts pool-facts">
        <div>
          <dt>已用</dt>
          <dd>{bytes(used)}</dd>
        </div>
        <div>
          <dt>上限</dt>
          <dd>{bytes(max)}</dd>
        </div>
        <div>
          <dt>清理目标</dt>
          <dd>{bytes(storage?.cleanup_target_bytes)}</dd>
        </div>
        <div>
          <dt>预留</dt>
          <dd>{bytes(storage?.reserve_bytes)}</dd>
        </div>
        <div>
          <dt>录音数</dt>
          <dd>{count(storage?.recordings)}</dd>
        </div>
        <div>
          <dt>目录</dt>
          <dd className="ellipsis" title={storage?.recording_dir ?? ''}>
            {storage?.recording_dir ?? '—'}
          </dd>
        </div>
      </dl>
      {storage?.storage_pressure && (
        <p className="card-note bad">
          容量压力：已按最旧优先回收。若持续出现，请提高上限或把 recording_dir 换到更大的卷。
        </p>
      )}
    </article>
  )
}

/**
 * The command lines behind the three processes.
 *
 * The server and the recogniser are described by the server itself: it knows its
 * own argv and the exact arguments it passes to the helper. The node is on
 * another host, so the only honest source is the node's own report — a nil
 * report means "that node does not tell us", not "it has no arguments".
 */
function LaunchPanel({ status }: { status: Status | null }) {
  const launch = status?.service.launch ?? null
  const node = status?.source.node_report ?? null
  const stt = status?.stt.launch ?? null
  const sttMode = status?.stt.mode

  return (
    <section className="panel launch-panel">
      <PanelHead title="启动参数" meta="server · node · 识别器" />
      <div className="launch-grid">
        <div className="launch-item">
          <h3>服务端</h3>
          {launch ? (
            <>
              <code className="launch-line" title={shellLine(launch.argv)}>
                {shellLine(launch.argv)}
              </code>
              <dl className="facts tight">
                <div>
                  <dt>配置</dt>
                  <dd className="ellipsis" title={launch.config_path ?? ''}>
                    {launch.config_path ?? '—'}
                  </dd>
                </div>
                <div>
                  <dt>PID</dt>
                  <dd>{launch.pid}</dd>
                </div>
              </dl>
            </>
          ) : (
            <p className="empty">这个服务端没有记录启动参数。</p>
          )}
        </div>

        <div className="launch-item">
          <h3>采集端（节点）</h3>
          {node ? (
            <>
              <code className="launch-line" title={shellLine(asArgv(node.argv))}>
                {shellLine(asArgv(node.argv))}
              </code>
              <dl className="facts tight">
                <div>
                  <dt>配置</dt>
                  <dd className="ellipsis" title={asText(node.config) ?? ''}>
                    {asText(node.config) ?? '—'}
                  </dd>
                </div>
                <div>
                  <dt>版本</dt>
                  <dd>{asText(node.version) ?? '—'}</dd>
                </div>
                <div>
                  <dt>上报</dt>
                  <dd>{ageText(status?.source.node_report_at_utc ?? null)}</dd>
                </div>
                <div>
                  <dt>朝向</dt>
                  <dd className="ellipsis" title={asText(node.server) ?? ''}>
                    {asText(node.server) ?? '—'}
                  </dd>
                </div>
              </dl>
            </>
          ) : (
            <p className="empty">
              节点还没有上报过自己的命令行。旧版本的 node 不会发这个包，重新部署
              `steno-node` 后就会出现。
            </p>
          )}
        </div>

        <div className="launch-item">
          <h3>识别器（{sttMode === 'segment' ? '逐段' : '流式'}）</h3>
          {stt ? (
            <>
              <code className="launch-line" title={shellLine([stt.command, ...stt.args])}>
                {shellLine([stt.command, ...stt.args])}
              </code>
              <dl className="facts tight">
                <div>
                  <dt>引擎</dt>
                  <dd>{status?.stt.engine || '—'}</dd>
                </div>
                <div>
                  <dt>状态</dt>
                  <dd>{status ? sttStateLabel(status.stt).text : '—'}</dd>
                </div>
              </dl>
              <p className="card-note">
                实际执行的是 `~/.local/bin/{stt.command}`，它再用 LaunchServices
                打开 .app（TCC 授权只认 bundle）。
              </p>
            </>
          ) : (
            <p className="empty">转写未开启，没有在跑的识别器进程。</p>
          )}
        </div>
      </div>
    </section>
  )
}

function UnitActions({
  target,
  control,
  busy,
  act,
}: {
  target: ControlTarget
  control: ControlView | null
  busy: string | null
  act: (target: ControlTarget, action: ControlAction) => void
}) {
  // `null` means the probe never answered — an older server that has no
  // /control endpoint, or one that is momentarily unreachable. Saying "disabled"
  // there would blame the config for a missing route.
  if (!control) {
    return <p className="card-note">读不到启停状态：接口未响应。</p>
  }
  if (!control.enabled) {
    return (
      <p className="card-note">
        启停未启用（<code>[control] enabled = false</code>）。
      </p>
    )
  }
  if (!control.available) {
    // Deliberately silent: on a host without systemd the reason is a property of
    // the machine, not news, and a paragraph saying so sat under two different
    // cards on every page load. The actions are simply not offered.
    return null
  }
  const unit = control.units.find((item) => item.target === target)
  return (
    <>
      {unit && (
        <p className="card-note">
          单元 <code>{unit.unit}</code> · {unitStateLabel(unit.state, true).text}
        </p>
      )}
      <div className="card-actions">
        {(['start', 'stop', 'restart'] as ControlAction[]).map((action) => (
          <button
            key={action}
            onClick={() => act(target, action)}
            disabled={busy !== null}
            className={busy === `${target}:${action}` ? 'on' : ''}
          >
            {actionLabel(action)}
          </button>
        ))}
      </div>
    </>
  )
}

// --- vocabulary ------------------------------------------------------------

/**
 * The server card's state is the API's own answer: this page is talking to the
 * server, so "it answered" is the strongest liveness signal there is. The
 * systemd unit is only needed to start it, and is shown next to the buttons.
 */
function serverState(status: Status | null, everLoaded: boolean): { text: string; tone: Tone } {
  if (!status) {
    return everLoaded ? { text: '不可达', tone: 'bad' } : { text: '连接中', tone: 'idle' }
  }
  if (status.service.state === 'degraded') return { text: '降级', tone: 'warn' }
  if (status.recording.state === 'stopped') return { text: '已停止', tone: 'warn' }
  return { text: '运行中', tone: 'ok' }
}

/**
 * The node card's state is the heartbeat: HELLO plus a packet watcher with an
 * idle timeout. Nothing about systemd is involved in "is the bedroom device
 * sending audio".
 */
function nodeState(status: Status | null): { text: string; tone: Tone } {
  if (!status) return { text: '未知', tone: 'idle' }
  return status.source.node_connected
    ? { text: '在线', tone: 'ok' }
    : { text: '离线', tone: 'warn' }
}

/**
 * How much audio the node chose not to send. It is shown next to the loss counters
 * on purpose: the whole point of the gate protocol is that these two numbers are
 * different, and an operator needs to see that the node is behaving.
 */
function gatedText(status: Status | null): string {
  if (!status) return '—'
  const frames = status.source.gated_frames
  const rate = status.microphone.capture_rate_hz
  if (!frames || !rate) return '—'
  return duration((frames / rate) * 1000)
}

/**
 * The recording format is always 16 kHz mono, so showing it alone says nothing.
 * What an operator actually needs is the pair: what is stored, and what the
 * microphone (or the node's microphone) runs at. They differ whenever the device
 * needed converting, and that difference is the first thing to check when a
 * transcript comes out stretched.
 */
function formatSummary(status: Status | null): string {
  const mic = status?.microphone
  if (!mic || !mic.capture_rate_hz) return '—'
  const recorded = `${mic.capture_rate_hz} Hz · ${mic.channels} ch`
  if (!mic.device_rate_hz) return recorded
  if (
    mic.device_rate_hz === mic.capture_rate_hz &&
    mic.device_channels === mic.channels
  ) {
    return recorded
  }
  return `${mic.device_rate_hz} Hz · ${mic.device_channels} ch → ${recorded}`
}

function formatTitle(status: Status | null): string {
  const mic = status?.microphone
  if (!mic || !mic.capture_rate_hz) return ''
  if (!mic.device_rate_hz) return `录制格式 ${mic.capture_rate_hz} Hz · ${mic.channels} ch`
  return `设备 ${mic.device_rate_hz} Hz · ${mic.device_channels} ch，录制 ${mic.capture_rate_hz} Hz · ${mic.channels} ch`
}

function micLabel(status: Status | null): string {
  if (!status) return '未知'
  switch (status.microphone.state) {
    case 'ok':
      return '采集中'
    case 'disconnected':
      return '未连接'
    case 'degraded':
      return '降级'
    default:
      return '未知'
  }
}

function micTone(status: Status | null): Tone {
  if (!status) return 'idle'
  return status.microphone.state === 'ok' ? 'ok' : 'warn'
}

function recordingLabel(state: string): string {
  switch (state) {
    case 'ok':
      return '正常'
    case 'degraded':
      return '降级'
    case 'stopped':
      return '已停止'
    case 'unknown':
      return '未知'
    default:
      return state
  }
}

/**
 * The pipeline reports `recording = ok` as soon as it is running, which is true
 * even when no node has ever sent a packet. Saying "正常" there would hide the
 * one failure this device exists to make visible.
 */
function recordingText(status: Status | null): string {
  if (!status) return '未知'
  if (status.source.kind === 'net' && !status.source.node_connected) return '等待采集端'
  return recordingLabel(status.recording.state)
}

/**
 * `signal_state` is `ok` whenever the pipeline is running and not silent, which
 * is true with no capture node attached — there is simply no audio to judge. In
 * that case the honest answer is "unknown", not "有信号".
 *
 * `dead` is the state that matters most: samples arrive, every byte-level
 * counter is healthy, and the audio carries only dither because the capsule is
 * muted or the mic is unseated. It used to read as `有信号`, which is how a
 * silent microphone spent hours looking like a working one.
 */
function signalText(status: Status | null): string {
  if (!status) return '未知'
  if (status.source.kind === 'net' && !status.source.node_connected) return '未知'
  switch (status.signal_state) {
    case 'ok':
      return '有信号'
    case 'silent':
      return '静默（无数据流）'
    case 'dead':
      return '无可用信号'
    default:
      return '未知'
  }
}

/** One line explaining what to do about a signal problem, or null when fine. */
function signalHint(status: Status | null): string | null {
  if (!status) return null
  const peak = status.capture.level_peak_dbfs
  if (status.signal_state === 'dead') {
    return peak == null
      ? '麦克风没有拾音：检查话筒的静音开关、话筒是否插在话筒座上、输入增益是否为 0。'
      : `麦克风没有拾音（峰值 ${peak.toFixed(1)} dBFS）：检查静音开关、话筒是否插在话筒座上、输入增益是否为 0。`
  }
  if (status.signal_state === 'silent') {
    return '采集端在发数据，但每个采样都是 0：检查设备选择与采集端用户是否在 audio 组。'
  }
  return null
}
