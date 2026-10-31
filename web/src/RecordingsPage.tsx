// The recordings page: a day of audio, one bar, one player.
//
// This page owns everything that is scoped to a date — the day strip, the
// timeline, candidate segments, the player and the transcript. It asks the
// server for `/status` too, but only to phrase the transcript's empty state
// correctly ("转写未开启" versus "今天还没有识别到语音"); it never shows an
// operational card, because that is the console's job.

import { useEffect, useMemo, useRef, useState } from 'react'
import {
  api,
  duration,
  localClock,
  localTime,
  shiftDate,
  todayISO,
  type CandidateEvent,
  type Day,
  type Segment,
  type Status,
  type Utterance,
} from './api'
import { Empty, PanelHead, timeQualityLabel } from './ui'
import { useLiveTranscript } from './liveTranscript'

export default function RecordingsPage() {
  const [date, setDate] = useState(todayISO())
  const [status, setStatus] = useState<Status | null>(null)
  const [days, setDays] = useState<Day[]>([])
  const [segments, setSegments] = useState<Segment[]>([])
  const [events, setEvents] = useState<CandidateEvent[]>([])
  const [selectedSegment, setSelectedSegment] = useState<string | null>(null)
  const [utterances, setUtterances] = useState<Utterance[]>([])
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    api
      .status()
      .then((next) => !cancelled && setStatus(next))
      .catch(() => !cancelled && setStatus(null))
    return () => {
      cancelled = true
    }
  }, [])

  useEffect(() => {
    let cancelled = false
    setError(null)
    Promise.all([api.segments(date), api.events(date), api.utterances(date)])
      .then(([nextSegments, nextEvents, nextUtterances]) => {
        if (cancelled) return
        setSegments(nextSegments)
        setEvents(nextEvents)
        setUtterances(nextUtterances)
        // Open on the first segment that actually has audio: the server opens a
        // segment as soon as the pipeline starts, so segment 0 of a day is
        // often an empty placeholder whose file never existed.
        const firstWithAudio =
          nextSegments.find((item) => item.status === 'complete' && item.duration_ms > 0) ??
          nextSegments[0]
        setSelectedSegment(firstWithAudio?.id ?? null)
      })
      .catch((cause: Error) => !cancelled && setError(cause.message))
    return () => {
      cancelled = true
    }
  }, [date])

  useEffect(() => {
    api
      .days(shiftDate(date, -30), date)
      .then(setDays)
      .catch(() => setDays([]))
  }, [date])

  const isToday = date === todayISO()
  // The socket is always "now", so it is only wired up while the page shows
  // today. On any other date the panel is purely historical. And with `[stt]
  // enabled = false` the server answers 503 for the upgrade: better not to open
  // a socket that can only fail and then retry forever with backoff.
  const live = useLiveTranscript(isToday && (status?.stt.enabled ?? false))
  // History + whatever arrived over the socket since the page opened. Deduped by
  // id because a phrase can be both: fetched as history and pushed as a final.
  const allUtterances = useMemo(() => {
    const seen = new Set(utterances.map((item) => item.id))
    return [...utterances, ...live.finals.filter((item) => !seen.has(item.id))].sort((a, b) =>
      a.started_at_utc.localeCompare(b.started_at_utc),
    )
  }, [utterances, live.finals])

  const spokenMs = allUtterances.reduce(
    (sum, item) => sum + Math.max(Date.parse(item.ended_at_utc) - Date.parse(item.started_at_utc), 0),
    0,
  )
  const segment = segments.find((item) => item.id === selectedSegment) ?? null
  const segmentEvents = useMemo(
    () => events.filter((event) => event.segment_id === segment?.id),
    [events, segment],
  )

  const review = async (event: CandidateEvent, next: CandidateEvent['review_status']) => {
    const previous = event.review_status
    setEvents((current) =>
      current.map((item) => (item.id === event.id ? { ...item, review_status: next } : item)),
    )
    try {
      await api.review(event.id, next)
    } catch (cause) {
      setError((cause as Error).message)
      setEvents((current) =>
        current.map((item) =>
          item.id === event.id ? { ...item, review_status: previous } : item,
        ),
      )
    }
  }

  return (
    <>
      {error && <div className="banner error">接口错误：{error}</div>}

      <div className="daybar">
        <button className="ghost" onClick={() => setDate(shiftDate(date, -1))} aria-label="前一天">
          ←
        </button>
        <input
          type="date"
          value={date}
          max={todayISO()}
          onChange={(event) => setDate(event.target.value)}
        />
        <button
          className="ghost"
          onClick={() => setDate(shiftDate(date, 1))}
          disabled={date >= todayISO()}
          aria-label="后一天"
        >
          →
        </button>
        <div className="day-scroll">
          {days.map((day) => (
            <button
              key={day.date}
              className={`day-chip ${day.date === date ? 'active' : ''}`}
              onClick={() => setDate(day.date)}
              title={`${day.segment_count} 段 · ${duration(day.recorded_ms)}`}
            >
              <span>{day.date.slice(5)}</span>
              <b>{day.segment_count}</b>
            </button>
          ))}
        </div>
      </div>

      <section className="layout">
        <div className="panel timeline-panel">
          <PanelHead
            title="录音时间轴"
            meta={
              segments.length
                ? `${segments.length} 段 · ${duration(
                    segments.reduce((sum, item) => sum + item.duration_ms, 0),
                  )}`
                : '这一天没有录音'
            }
          />
          <NightTimeline
            segments={segments}
            events={events}
            selected={selectedSegment}
            onSelect={setSelectedSegment}
          />
        </div>

        <div className="panel candidates-panel">
          <PanelHead
            title="候选片段"
            meta={segmentEvents.length ? `${segmentEvents.length} 个` : '无'}
          />
          {segmentEvents.length === 0 && (
            <Empty text="这一段没有候选片段。检测器只做标注，不决定记录什么。" />
          )}
          <ul className="event-list">
            {segmentEvents.map((event) => (
              <li key={event.id} className={`event ${event.review_status}`}>
                <div className="event-time">{localTime(event.started_at_utc)}</div>
                <div className="event-body">
                  <div className="event-meta">
                    {(event.duration_ms / 1000).toFixed(1)} 秒
                    {event.peak_dbfs !== null && <> · 峰值 {event.peak_dbfs.toFixed(1)} dBFS</>}
                    {event.continued && <> · 连续</>}
                  </div>
                  <div className="event-actions">
                    <button
                      className={event.review_status === 'snore' ? 'on' : ''}
                      onClick={() => review(event, 'snore')}
                      title="标记为有效片段"
                    >
                      保留
                    </button>
                    <button
                      className={event.review_status === 'not_snore' ? 'on' : ''}
                      onClick={() => review(event, 'not_snore')}
                      title="标记为无效片段"
                    >
                      排除
                    </button>
                  </div>
                </div>
              </li>
            ))}
          </ul>
        </div>

        <div className="panel detail-panel">
          <PanelHead
            title="录音详情"
            meta={
              segment
                ? `${localClock(segment.started_at_utc)} 起 · ${duration(segment.duration_ms)}`
                : '选择一段录音'
            }
          />
          {segment ? (
            <>
              {/* `metadata` (not `none`) so the player shows a real duration:
                  the header is two range requests, and "0:00 / 0:00" read as a
                  broken segment. */}
              <audio controls preload="metadata" src={segment.audio_url} className="player" />
              <dl className="facts">
                <div>
                  <dt>状态</dt>
                  <dd>{segmentLabel(segment.status)}</dd>
                </div>
                <div>
                  <dt>时间质量</dt>
                  <dd>{timeQualityLabel(segment.time_quality)}</dd>
                </div>
                <div>
                  <dt>丢帧</dt>
                  <dd>{segment.lost_frames_total.toLocaleString()}</dd>
                </div>
                <div>
                  <dt>时钟漂移</dt>
                  <dd>
                    {segment.clock_drift_ppm === null
                      ? '—'
                      : `${segment.clock_drift_ppm.toFixed(1)} ppm`}
                  </dd>
                </div>
              </dl>
            </>
          ) : (
            <Empty text="还没有选中录音。" />
          )}
        </div>
      </section>

      <section className="panel utterances-panel">
        <PanelHead
          title={`${date} 转录`}
          meta={
            // `allUtterances`, not `utterances`: the header has to count what the
            // body shows, including phrases that arrived over the socket after
            // the day was fetched.
            status?.stt.enabled
              ? `${status.stt.engine} · ${allUtterances.length} 句 · ${duration(spokenMs)}`
              : `${allUtterances.length} 句 · 转写未开启`
          }
        />
        {allUtterances.length === 0 && !(isToday && live.partial) && (
          <Empty text={transcriptEmptyHint(status)} />
        )}
        {/* One continuous passage, not a log. A transcript is read as prose; the
            timestamps stay available on hover and as a per-phrase anchor so a
            reader can still jump to a moment. */}
        {allUtterances.length > 0 && (
          <div className="transcript">
            {allUtterances.map((item) => (
              <span
                key={item.id}
                className="transcript-phrase"
                title={`${localTime(item.started_at_utc)} — ${localTime(item.ended_at_utc)}`}
              >
                {item.text}
              </span>
            ))}
          </div>
        )}
        {/* The live half-sentence belongs to *now*, so it is only shown on today.
            It comes off the socket, not off a poll: a 4 s poll showed it stuttering
            and could miss a short phrase entirely. */}
        {isToday && live.partial && (
          <p className="transcript-live">
            <span className="transcript-live-mark">正在识别</span>
            {live.partial}
          </p>
        )}
        {status?.stt.error && <div className="banner error">识别器：{status.stt.error}</div>}
        {isToday && <StreamNote live={live} />}
        {allUtterances.length > 0 && (
          <p className="transcript-foot">
            共 {allUtterances.length} 句，覆盖 {duration(spokenMs)}；每句的起止时间在悬停时显示。
          </p>
        )}
      </section>
    </>
  )
}

// A night is drawn as one continuous bar: segments are blocks and candidate
// events are ticks on top, so a gap in coverage is visible as a gap rather than
// as a number the reader has to interpret.
//
// Zooming is a *viewport* over the same bar, not a re-layout: the scale factor
// multiplies the track's width and the scroll offset decides which slice is
// visible. That keeps the geometry honest — a 10 s segment stays 10 s wide
// relative to its neighbours at every zoom — and it is the cheapest thing that
// works, because nothing has to be recomputed per zoom step.
const MIN_ZOOM = 1
const MAX_ZOOM = 240

function NightTimeline({
  segments,
  events,
  selected,
  onSelect,
}: {
  segments: Segment[]
  events: CandidateEvent[]
  selected: string | null
  onSelect: (id: string) => void
}) {
  const trackRef = useRef<HTMLDivElement>(null)
  const [zoom, setZoom] = useState(1)
  // The visible window is the track's own scroll, so the browser handles
  // dragging, momentum and the scrollbar; we only clamp zoom against it.
  const [dragging, setDragging] = useState(false)

  if (segments.length === 0) {
    return <Empty text="没有可显示的录音段。" />
  }

  const start = Math.min(...segments.map((item) => Date.parse(item.started_at_utc)))
  const end = Math.max(
    ...segments.map((item) => Date.parse(item.ended_at_utc ?? item.started_at_utc) || Date.now()),
  )
  const span = Math.max(end - start, 1)
  const position = (time: number) => `${((time - start) / span) * 100}%`

  /**
   * Zooms around the cursor: the instant under the pointer stays put.
   *
   * Takes the *factor* rather than the target zoom, because `zoom` is captured
   * per render: three clicks dispatched in one frame all read the same stale
   * value and only the last one counted. The functional updater always sees the
   * current state, so repeated clicks compose.
   */
  const zoomBy = (factor: number, anchorRatio: number) => {
    const track = trackRef.current
    const anchorPx = track ? track.scrollLeft + anchorRatio * track.clientWidth : 0
    const viewport = track?.clientWidth ?? 0
    setZoom((current) => {
      const next = Math.min(Math.max(current * factor, MIN_ZOOM), MAX_ZOOM)
      if (next === current) return current
      if (track) {
        // Keep the anchored instant at the same pixel: the scroll offset moves
        // by the same *proportion*, not the same number of pixels.
        requestAnimationFrame(() => {
          track.scrollLeft = anchorPx * (next / current) - anchorRatio * viewport
        })
      }
      return next
    })
  }

  const resetZoom = () => {
    const track = trackRef.current
    setZoom(1)
    if (track) requestAnimationFrame(() => (track.scrollLeft = 0))
  }

  const onWheel = (event: React.WheelEvent<HTMLDivElement>) => {
    // Ctrl/⌘ + wheel is the convention for zoom inside a scroll container;
    // a bare wheel keeps scrolling so the track stays usable on a trackpad.
    if (!(event.ctrlKey || event.metaKey)) return
    event.preventDefault()
    const rect = event.currentTarget.getBoundingClientRect()
    const ratio = (event.clientX - rect.left) / Math.max(rect.width, 1)
    zoomBy(event.deltaY < 0 ? 1.25 : 0.8, ratio)
  }

  const onPointerDown = (event: React.PointerEvent<HTMLDivElement>) => {
    // Buttons (segments) handle their own clicks; only drag the bare track.
    if (event.button !== 0) return
    const track = event.currentTarget
    const originX = event.clientX
    const originScroll = track.scrollLeft
    let moved = false
    const move = (next: PointerEvent) => {
      const delta = next.clientX - originX
      if (Math.abs(delta) > 3) moved = true
      track.scrollLeft = originScroll - delta
    }
    const up = () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
      setDragging(false)
      // A drag that ends over a segment must not also select it.
      if (moved) event.preventDefault()
    }
    setDragging(true)
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
  }

  const visibleLabel = zoom === 1 ? '全天' : `${zoom.toFixed(1)}×`

  return (
    <div className="timeline">
      <div className="timeline-axis">
        {/* A 40-second window labelled only in hours:minutes prints the same
            clock three times, so short spans get seconds. */}
        <span>{clockOf(start, span)}</span>
        <span>{clockOf(start + span / 2, span)}</span>
        <span>{clockOf(end, span)}</span>
      </div>

      <div className="timeline-bar">
        <div className="timeline-zoom">
          <button
            onClick={() => zoomBy(0.8, 0.5)}
            disabled={zoom <= MIN_ZOOM}
            aria-label="缩小"
            title="缩小（⌘/Ctrl + 滚轮）"
          >
            −
          </button>
          <span className="timeline-zoom-label" title="当前缩放">
            {visibleLabel}
          </span>
          <button
            onClick={() => zoomBy(1.25, 0.5)}
            disabled={zoom >= MAX_ZOOM}
            aria-label="放大"
            title="放大（⌘/Ctrl + 滚轮）"
          >
            +
          </button>
          <button
            className="timeline-reset"
            onClick={resetZoom}
            disabled={zoom === 1}
            title="回到全天（双击时间轴也行）"
          >
            复位
          </button>
        </div>
        <p className="timeline-hint">
          {zoom === 1
            ? '⌘/Ctrl + 滚轮缩放，拖动平移，双击复位'
            : `已放大 ${zoom.toFixed(1)}×，拖动平移，双击复位`}
        </p>
      </div>

      <div
        ref={trackRef}
        className={`timeline-scroll ${dragging ? 'dragging' : ''}`}
        onWheel={onWheel}
        onPointerDown={onPointerDown}
        onDoubleClick={resetZoom}
      >
        <div className="timeline-track" style={{ width: `${zoom * 100}%` }}>
          {segments.map((item) => {
            const from = Date.parse(item.started_at_utc)
            const to = Date.parse(item.ended_at_utc ?? item.started_at_utc)
            return (
              <button
                key={item.id}
                className={`segment ${item.id === selected ? 'selected' : ''} ${item.status}`}
                style={{ left: position(from), width: `${((to - from) / span) * 100}%` }}
                title={`${localClock(item.started_at_utc)} · ${duration(item.duration_ms)}`}
                onClick={() => onSelect(item.id)}
              />
            )
          })}
          {events.map((event) => (
            <span
              key={event.id}
              className={`tick ${event.review_status}`}
              style={{ left: position(Date.parse(event.started_at_utc)) }}
              title={`${localTime(event.started_at_utc)} · ${(event.duration_ms / 1000).toFixed(1)}s`}
            />
          ))}
        </div>
      </div>

    </div>
  )
}

/** Clock label for one timeline axis point; seconds when the window is short. */
function clockOf(time: number, span: number): string {
  const iso = new Date(time).toISOString()
  return span < 10 * 60 * 1000 ? localTime(iso) : localClock(iso)
}

/**
 * The transcript panel is day-scoped (`/api/v1/utterances?date=`), so its empty
 * state must talk about the day. It used to say "这一段" while the query ignored
 * the selected segment entirely.
 */
function transcriptEmptyHint(status: Status | null): string {
  if (!status) return '服务端不可达，读不到今天的语句。'
  const stt = status.stt
  if (!stt.enabled) {
    return '转写未开启：在配置里写 [stt] enabled = true 并重启服务端后，说话的文本会出现在这里。'
  }
  if (stt.state === 'error') {
    return '识别器没有起来，原因见下方错误行。'
  }
  if (stt.state === 'starting') {
    return '识别器正在连接；连上后说话的文本会出现在这里。'
  }
  if (stt.state === 'segment') {
    return '逐段转写模式：录音段结束后才会有文本，这里暂时还没有。'
  }
  return '今天还没有识别到语音。'
}

/**
 * One line about the socket itself.
 *
 * It exists because "no text yet" has several causes and only one of them is
 * silence. Without this the panel is indistinguishable between "the room is
 * quiet", "the stream never connected" and "this browser missed a minute".
 */
function StreamNote({ live }: { live: ReturnType<typeof useLiveTranscript> }) {
  if (live.status === 'live' && live.lagged === 0 && !live.error) {
    return (
      <p className="stream-note">
        <span className="stream-dot live" />
        实时 · {live.engine ?? '识别器'}
        {live.historyAtConnect !== null && ` · 连接时已有 ${live.historyAtConnect} 句`}
      </p>
    )
  }
  if (live.error) {
    return (
      <p className="stream-note bad">
        <span className="stream-dot bad" />
        转写流出错：{live.error}
      </p>
    )
  }
  if (live.lagged > 0) {
    return (
      <p className="stream-note warn">
        <span className="stream-dot warn" />
        落后了 {live.lagged} 条事件，中间有句子没显示；刷新可补齐。
      </p>
    )
  }
  return (
    <p className="stream-note">
      <span className="stream-dot" />
      {live.status === 'reconnecting' ? '正在重连转写流…' : '正在连接转写流…'}
    </p>
  )
}

function segmentLabel(status: string): string {
  switch (status) {
    case 'complete':
      return '完整'
    case 'recording':
      return '录音中'
    case 'interrupted':
      return '中断（已恢复）'
    case 'missing':
      return '文件缺失'
    default:
      return status
  }
}
