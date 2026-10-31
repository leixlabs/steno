// The recordings page: a day of audio, one bar, one player.
//
// This page owns everything that is scoped to a date — the day strip, the
// timeline, candidate segments, the player and the transcript. It asks the
// server for `/status` too, but only to phrase the transcript's empty state
// correctly ("转写未开启" versus "今天还没有识别到语音"); it never shows an
// operational card, because that is the console's job.
//
// Three selections talk to each other, and each one answers a different
// question: the day strip picks a *date*, the timeline picks a *window* on the
// night, and the transcript picks an *instant*. A sentence is a jump target, so
// clicking one seeks the player and selects the segment that holds it; a window
// is a measurement, so its summary lives in the timeline's own panel head —
// "how much of the night was this" is only ever asked while looking at the
// selection, and a separate card for it would outlive the selection.

import { useEffect, useMemo, useRef, useState } from 'react'
import {
  api,
  duration,
  localClock,
  localTime,
  share,
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

/** A window on the night. Epoch milliseconds, because every sum here is a subtraction. */
interface TimeRange {
  from: number
  to: number
}

/** Consecutive phrases joined into one readable row. */
interface Passage {
  /** The first phrase's id: stable across renders and unique on the page. */
  id: string
  phrases: Utterance[]
  from: number
  to: number
  /** Sum of the phrases' own durations — not the span, which includes silence. */
  spokenMs: number
}

/**
 * Silence longer than this ends a passage.
 *
 * The recogniser closes a final whenever it hears a break, so one remark can
 * arrive as four rows of three characters ("我吗 / 我也 / 嗯。") — complete, and
 * unreadable as a column. A minute is the practical definition of "something
 * else was said later" for a night log: at that threshold tonight's 296 phrases
 * become 152 rows, and inside a conversation the effect is much larger (19
 * finals across three minutes become one row). A vocalisation minutes away from
 * the last one keeps its own row — it *is* a different moment.
 *
 * This is a display rule, not a claim about grammar: nothing here invents
 * punctuation, and the row's hover text still carries the exact clock.
 */
const PASSAGE_GAP_MS = 60_000
/** …but one row still has to stay readable, so a monologue becomes several. */
const PASSAGE_MAX_MS = 120_000
const PASSAGE_MAX_PHRASES = 32

const DAY_FILTERS = [
  { id: 'all', label: '全部' },
  { id: 'recorded', label: '有录音' },
  { id: 'events', label: '有候选' },
] as const

type DayFilter = (typeof DAY_FILTERS)[number]['id']

export default function RecordingsPage() {
  const [date, setDate] = useState(todayISO())
  const [status, setStatus] = useState<Status | null>(null)
  const [days, setDays] = useState<Day[]>([])
  const [segments, setSegments] = useState<Segment[]>([])
  const [events, setEvents] = useState<CandidateEvent[]>([])
  const [selectedSegment, setSelectedSegment] = useState<string | null>(null)
  const [utterances, setUtterances] = useState<Utterance[]>([])
  const [error, setError] = useState<string | null>(null)
  // The window picked on the timeline, and how the day strip is narrowed. Both
  // are view state, so neither survives a date change — a window on Tuesday
  // means nothing on Wednesday.
  const [range, setRange] = useState<TimeRange | null>(null)
  const [dayFilter, setDayFilter] = useState<DayFilter>('all')
  /** Where the player should be after a jump; consumed by `onLoadedMetadata`. */
  const pendingSeek = useRef<{ segmentId: string; offsetMs: number } | null>(null)
  const [playhead, setPlayhead] = useState<number | null>(null)
  const [jumpNote, setJumpNote] = useState<string | null>(null)
  const audioRef = useRef<HTMLAudioElement>(null)

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
    setRange(null)
    setJumpNote(null)
    setPlayhead(null)
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

  // ← / → walk the calendar and Esc drops the window. They are guarded against
  // form fields so the date input keeps its own arrow behaviour.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null
      if (target && /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName)) return
      if (event.key === 'ArrowLeft') setDate((current) => shiftDate(current, -1))
      if (event.key === 'ArrowRight') {
        setDate((current) => (current >= todayISO() ? current : shiftDate(current, 1)))
      }
      if (event.key === 'Escape') setRange(null)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

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
  // A playhead measured inside one segment means nothing inside the next one.
  useEffect(() => {
    setPlayhead(null)
  }, [segment?.id])
  // Which stretches of the night still have a file behind them. Retention keeps
  // the rows and the transcript while the audio goes, so a sentence is only a
  // jump target when its stretch survived — and saying so before the click is
  // cheaper than explaining it after. Merged, because two neighbouring segments
  // can cover the same instant and a lookup that assumes disjoint spans would
  // then miss.
  const playableSpans = useMemo(() => {
    const spans = segments
      .filter((item) => playableStatus(item.status))
      .map((item) => {
        const start = Date.parse(item.started_at_utc)
        const end = Date.parse(item.ended_at_utc ?? '') || start + item.duration_ms
        return { start, end }
      })
      .sort((a, b) => a.start - b.start)
    const merged: { start: number; end: number }[] = []
    for (const span of spans) {
      const last = merged[merged.length - 1]
      if (last && span.start <= last.end) last.end = Math.max(last.end, span.end)
      else merged.push({ ...span })
    }
    return merged
  }, [segments])
  const segmentEvents = useMemo(
    () => events.filter((event) => event.segment_id === segment?.id),
    [events, segment],
  )

  /** What the selected window contains. `null` when nothing is selected. */
  const rangeStats = useMemo(() => {
    if (!range) return null
    const from = Math.min(range.from, range.to)
    const to = Math.max(range.from, range.to)
    if (to - from < 500) return null
    const clip = (start: number, end: number) =>
      Math.max(Math.min(end, to) - Math.max(start, from), 0)

    let recordedMs = 0
    let segmentCount = 0
    for (const item of segments) {
      const start = Date.parse(item.started_at_utc)
      const end = Date.parse(item.ended_at_utc ?? '') || start + item.duration_ms
      const inside = clip(start, end)
      if (inside > 0) {
        segmentCount += 1
        recordedMs += inside
      }
    }

    let eventCount = 0
    for (const event of events) {
      const at = Date.parse(event.started_at_utc)
      if (at >= from && at <= to) eventCount += 1
    }

    let utteranceCount = 0
    let spoken = 0
    for (const item of allUtterances) {
      const inside = clip(Date.parse(item.started_at_utc), Date.parse(item.ended_at_utc))
      if (inside > 0) {
        utteranceCount += 1
        spoken += inside
      }
    }

    return { from, to, ms: to - from, segmentCount, recordedMs, eventCount, utteranceCount, spokenMs: spoken }
  }, [range, segments, events, allUtterances])

  /**
   * A transcript is built from phrases but read in passages. The recogniser
   * emits a final every few seconds, so one remark arrives as several rows of
   * two or three characters — "我吗 / 我也 / 嗯。" is complete and unreadable.
   * See `buildPassages` for the rule.
   */
  const passages = useMemo(() => buildPassages(allUtterances), [allUtterances])

  /** The passage the player is currently inside. */
  const playingPassageId = useMemo(() => {
    if (playhead === null) return null
    let found: string | null = null
    for (const passage of passages) {
      if (passage.from > playhead) break
      found = passage.id
    }
    return found
  }, [playhead, passages])

  /**
   * Jumps to the audio behind one sentence: selects the segment that holds it
   * and seeks the player to the offset. Retention can reclaim a segment while
   * its transcript stays in the database, so "no segment" is a real answer and
   * is said out loud rather than silently doing nothing.
   */
  const jumpTo = (item: Utterance) => {
    const at = Date.parse(item.started_at_utc)
    const host = segments.find((candidate) => {
      if (!playableStatus(candidate.status)) return false
      const start = Date.parse(candidate.started_at_utc)
      const end = Date.parse(candidate.ended_at_utc ?? '') || start + candidate.duration_ms
      return at >= start && at < end
    })
    if (!host) {
      setJumpNote('这一句的录音不在本机：所在段已被容量回收或文件缺失，转写与候选记录仍然保留。')
      return
    }
    const offsetMs = Math.max(at - Date.parse(host.started_at_utc), 0)
    setJumpNote(null)
    setSelectedSegment(host.id)
    const player = audioRef.current
    if (player && host.id === segment?.id) {
      // Same src: nothing to wait for, so seek now.
      player.currentTime = offsetMs / 1000
      void player.play().catch(() => undefined)
      return
    }
    // New src: the media is not loaded yet, and seeking before `loadedmetadata`
    // is dropped. Hand the offset to the element's own event instead of guessing
    // a timeout.
    pendingSeek.current = { segmentId: host.id, offsetMs }
  }

  const onLoadedMetadata = () => {
    const player = audioRef.current
    const pending = pendingSeek.current
    if (!player || !pending) return
    pendingSeek.current = null
    player.currentTime = pending.offsetMs / 1000
    void player.play().catch(() => undefined)
  }

  const onTimeUpdate = () => {
    const player = audioRef.current
    if (!player || !segment) return
    setPlayhead(Date.parse(segment.started_at_utc) + player.currentTime * 1000)
  }

  // The day strip: the busiest day of the window sets the scale for every bar,
  // so a chip's bar means the same thing across the whole strip.
  const peakRecordedMs = Math.max(1, ...days.map((item) => item.recorded_ms))
  const visibleDays = days.filter(
    (item) => item.date === date || matchesDayFilter(item, dayFilter),
  )

  return (
    <>
      {error && <div className="banner error">接口错误：{error}</div>}

      <div className="daybar">
        <div className="day-nav">
          <button
            className="ghost"
            onClick={() => setDate(shiftDate(date, -1))}
            aria-label="前一天"
            title="前一天（←）"
          >
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
            title="后一天（→）"
          >
            →
          </button>
          <button className="ghost" onClick={() => setDate(todayISO())} disabled={isToday}>
            今天
          </button>
        </div>

        <div className="day-scroll">
          {visibleDays.map((item) => (
            <button
              key={item.date}
              className={[
                'day-chip',
                item.date === date ? 'active' : '',
                item.date === todayISO() ? 'today' : '',
                item.event_count > 0 ? 'has-events' : '',
              ]
                .filter(Boolean)
                .join(' ')}
              onClick={() => setDate(item.date)}
              title={[
                weekdayOf(item.date),
                `${item.segment_count} 段`,
                duration(item.recorded_ms),
                item.event_count ? `${item.event_count} 个候选` : '',
                item.lost_frames_total ? `丢帧 ${item.lost_frames_total.toLocaleString()}` : '',
              ]
                .filter(Boolean)
                .join(' · ')}
            >
              <span className="day-date">{item.date.slice(5)}</span>
              <span className="day-count">{item.segment_count > 0 ? item.segment_count : '—'}</span>
              {/* A bar, not a number: a month of nights is compared by shape, and
                  the count alone cannot tell a 20-minute night from an 8-hour one. */}
              <span className="day-bar" aria-hidden>
                <i style={{ width: `${share(item.recorded_ms, peakRecordedMs) * 100}%` }} />
              </span>
            </button>
          ))}
          {visibleDays.length === 0 && <span className="day-empty">这 30 天里没有符合条件的日期</span>}
        </div>

        <div className="day-filter" role="group" aria-label="按天筛选">
          {DAY_FILTERS.map((item) => (
            <button
              key={item.id}
              className={item.id === dayFilter ? 'on' : ''}
              onClick={() => setDayFilter(item.id)}
              aria-pressed={item.id === dayFilter}
            >
              {item.label}
            </button>
          ))}
        </div>
      </div>

      <section className="layout">
        <div className="panel timeline-panel">
          <div className="panel-head timeline-head">
            <h2>录音时间轴</h2>
            {rangeStats ? (
              <div className="head-stats">
                <span className="head-span">
                  {clockOfMs(rangeStats.from)} – {clockOfMs(rangeStats.to)}
                </span>
                <span>
                  时长 <b>{duration(rangeStats.ms)}</b>
                </span>
                <span>
                  录音 <b>{rangeStats.segmentCount}</b> 段 · <b>{duration(rangeStats.recordedMs)}</b>
                </span>
                <span>
                  候选 <b>{rangeStats.eventCount}</b>
                </span>
                <span>
                  语句 <b>{rangeStats.utteranceCount}</b> · <b>{duration(rangeStats.spokenMs)}</b>
                </span>
                <button className="ghost tiny" onClick={() => setRange(null)} title="清除区间（Esc）">
                  清除
                </button>
              </div>
            ) : (
              <span>
                {segments.length
                  ? `${segments.length} 段 · ${duration(
                      segments.reduce((sum, item) => sum + item.duration_ms, 0),
                    )}`
                  : '这一天没有录音'}
              </span>
            )}
          </div>
          <NightTimeline
            segments={segments}
            events={events}
            selected={selectedSegment}
            onSelect={setSelectedSegment}
            range={range}
            onRange={setRange}
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
                  {/* Labelling moved out of this page. Nights reviewed before
                      that still carry a label, and showing it beats pretending
                      every candidate was never looked at. */}
                  {event.review_status !== 'unreviewed' && (
                    <span className="event-tag">
                      {event.review_status === 'snore' ? '已标为候选' : '已排除'}
                    </span>
                  )}
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
                  broken segment. Retention deletes the audio but keeps the row,
                  so a reclaimed segment gets a sentence instead of a dead
                  player that looks like a bug. */}
              {playableStatus(segment.status) ? (
                <audio
                  ref={audioRef}
                  controls
                  preload="metadata"
                  src={segment.audio_url}
                  className="player"
                  onLoadedMetadata={onLoadedMetadata}
                  onTimeUpdate={onTimeUpdate}
                  onSeeked={onTimeUpdate}
                />
              ) : (
                <p className="empty player-missing">
                  这一段的声音已经不在了（{segmentLabel(segment.status)}）；时间轴、候选与转写仍然保留。
                </p>
              )}
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
        {/* One row per passage with its clock in a left rail: not one flowing
            block (a paragraph you cannot point at), and not one row per
            recogniser final either (a column of "嗯。" that is complete and
            unreadable). A merged row is still a jump target — it starts where
            its first phrase does. */}
        {passages.length > 0 && (
          <ol className="transcript">
            {passages.map((passage) => {
              const first = passage.phrases[0]
              const outside =
                rangeStats !== null &&
                (passage.to <= rangeStats.from || passage.from >= rangeStats.to)
              const reachable = hasAudio(playableSpans, passage.from)
              return (
                <li key={passage.id}>
                  <button
                    className={[
                      'transcript-row',
                      passage.id === playingPassageId ? 'active' : '',
                      outside ? 'outside' : '',
                      reachable ? '' : 'no-audio',
                    ]
                      .filter(Boolean)
                      .join(' ')}
                    onClick={() => jumpTo(first)}
                    title={`${passageTitle(passage)} · ${
                      reachable ? '点击跳到这一段的录音' : '这一段录音已被回收'
                    }`}
                  >
                    <span className="transcript-time">
                      <b>{localClock(first.started_at_utc)}</b>
                      <i>{duration(passage.spokenMs)}</i>
                    </span>
                    <span className="transcript-text">
                      {passage.phrases.map((item) => item.text).join(' ')}
                    </span>
                  </button>
                </li>
              )
            })}
          </ol>
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
        {jumpNote && <p className="transcript-note">{jumpNote}</p>}
        {status?.stt.error && <div className="banner error">识别器：{status.stt.error}</div>}
        {isToday && <StreamNote live={live} />}
        {passages.length > 0 && (
          <p className="transcript-foot">
            共 {allUtterances.length} 句，按停顿合并为 {passages.length} 条，覆盖{' '}
            {duration(spokenMs)}；点击任一条跳到它开头的录音，播放时会跟随高亮。
          </p>
        )}
      </section>
    </>
  )
}

// --- timeline ---------------------------------------------------------------

// A night is drawn as one continuous bar: segments are blocks and candidate
// events are ticks on top, so a gap in coverage is visible as a gap rather than
// as a number the reader has to interpret.
//
// Zooming is a *viewport* over the same bar, not a re-layout: the scale factor
// multiplies the track's width and the scroll offset decides which slice is
// visible. That keeps the geometry honest — a 10 s segment stays 10 s wide
// relative to its neighbours at every zoom — and it is the cheapest thing that
// works, because nothing has to be recomputed per zoom step.
//
// A drag on the bare track selects a window instead of panning it. Panning is
// still there, on Shift, and the track is a native scroll container anyway, so
// nothing was lost — but the *primary* drag is now the measurement, because that
// is the gesture an operator reaches for on a night chart.
const MIN_ZOOM = 1
const MAX_ZOOM = 240
/** Narrower than this a drag is a click, not a selection. */
const MIN_RANGE_MS = 1_000

function NightTimeline({
  segments,
  events,
  selected,
  onSelect,
  range,
  onRange,
}: {
  segments: Segment[]
  events: CandidateEvent[]
  selected: string | null
  onSelect: (id: string) => void
  range: TimeRange | null
  onRange: (next: TimeRange | null) => void
}) {
  const scrollRef = useRef<HTMLDivElement>(null)
  const trackRef = useRef<HTMLDivElement>(null)
  const [zoom, setZoom] = useState(1)
  // The visible window is the track's own scroll, so the browser handles
  // dragging, momentum and the scrollbar; we only clamp zoom against it.
  const [dragging, setDragging] = useState(false)
  /** The window while it is being dragged; committed to the page on pointerup. */
  const [draft, setDraft] = useState<TimeRange | null>(null)
  const draftRef = useRef<TimeRange | null>(null)
  const [selecting, setSelecting] = useState(false)
  /** Set when a drag ends, so the click it leaves behind does not select a block. */
  const swallowed = useRef(false)

  if (segments.length === 0) {
    return <Empty text="没有可显示的录音段。" />
  }

  const start = Math.min(...segments.map((item) => Date.parse(item.started_at_utc)))
  const end = Math.max(
    ...segments.map((item) => Date.parse(item.ended_at_utc ?? item.started_at_utc) || Date.now()),
  )
  const span = Math.max(end - start, 1)
  const position = (time: number) => `${((time - start) / span) * 100}%`
  const widthOf = (ms: number) => `${(ms / span) * 100}%`

  const show = (next: TimeRange | null) => {
    draftRef.current = next
    setDraft(next)
  }

  /** The instant under a client x, clamped to the track. */
  const timeAt = (clientX: number) => {
    const track = trackRef.current
    if (!track) return start
    const rect = track.getBoundingClientRect()
    const ratio = Math.min(Math.max((clientX - rect.left) / Math.max(rect.width, 1), 0), 1)
    return start + ratio * span
  }

  /**
   * Starts a window drag: a fresh one, or the resize of an existing one from
   * either edge.
   *
   * The press may land on a segment block — the blocks cover most of the track,
   * so demanding a bare strip would mean aiming at a few pixels. A press that
   * never moves is still a segment click (the block's own handler), so the two
   * gestures share the press and are told apart by distance: past a few pixels
   * it is a drag, and the click that follows is swallowed.
   */
  const beginRange = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0 || event.shiftKey) return
    const target = event.target as HTMLElement
    if (!target.closest('.timeline-track')) return
    const handle = target.closest<HTMLElement>('.range-handle')
    const onSegment = target.closest('.segment') !== null
    // A drag from the previous gesture may have left this armed.
    swallowed.current = false

    // Both would otherwise steal the gesture: the scroll container would start a
    // pan and the browser would start a text selection.
    event.preventDefault()
    event.stopPropagation()

    const originX = event.clientX
    const origin = timeAt(originX)
    const edge = handle?.dataset.edge as 'from' | 'to' | undefined
    const anchor = range
    const shape = (time: number): TimeRange => {
      if (edge === 'from' && anchor) {
        return { from: Math.min(time, anchor.to - MIN_RANGE_MS), to: anchor.to }
      }
      if (edge === 'to' && anchor) {
        return { from: anchor.from, to: Math.max(time, anchor.from + MIN_RANGE_MS) }
      }
      return { from: origin, to: time }
    }

    let moved = false
    const move = (next: PointerEvent) => {
      if (!moved && Math.abs(next.clientX - originX) < 3) return
      if (!moved) setSelecting(true)
      moved = true
      show(shape(timeAt(next.clientX)))
    }
    const up = () => {
      window.removeEventListener('pointermove', move)
      window.removeEventListener('pointerup', up)
      setSelecting(false)
      const settled = moved ? draftRef.current : null
      show(null)
      if (!settled) {
        // A press that did not move: on bare track that clears the window, on a
        // segment it is the block's own click and must not touch the window.
        if (!onSegment) onRange(null)
        return
      }
      swallowed.current = true
      const from = Math.min(settled.from, settled.to)
      const to = Math.max(settled.from, settled.to)
      onRange(to - from < MIN_RANGE_MS ? null : { from, to })
    }
    window.addEventListener('pointermove', move)
    window.addEventListener('pointerup', up)
  }

  /**
   * Zooms around the cursor: the instant under the pointer stays put.
   *
   * Takes the *factor* rather than the target zoom, because `zoom` is captured
   * per render: three clicks dispatched in one frame all read the same stale
   * value and only the last one counted. The functional updater always sees the
   * current state, so repeated clicks compose.
   */
  const zoomBy = (factor: number, anchorRatio: number) => {
    const track = scrollRef.current
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
    const track = scrollRef.current
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

  /** Shift + drag pans, because a bare drag now means "select a window". */
  const onPanStart = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0 || !event.shiftKey) return
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
  const shown = draft ? normalize(draft) : range
  const preview = draft ? normalize(draft) : null
  const hint = preview
    ? `选中 ${clockOfMs(preview.from)} – ${clockOfMs(preview.to)} · ${duration(
        preview.to - preview.from,
      )}`
    : zoom === 1
      ? '拖动框选区间 · Shift 拖动平移 · ⌘/Ctrl 滚轮缩放'
      : `已放大 ${zoom.toFixed(1)}× · 拖动框选 · Shift 平移 · 双击复位`

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
        <p className={`timeline-hint ${draft ? 'selecting' : ''}`}>{hint}</p>
      </div>

      <div
        ref={scrollRef}
        className={`timeline-scroll ${dragging ? 'dragging' : ''} ${selecting ? 'selecting' : ''}`}
        onWheel={onWheel}
        onPointerDown={onPanStart}
        onDoubleClick={resetZoom}
      >
        <div
          ref={trackRef}
          className="timeline-track"
          style={{ width: `${zoom * 100}%` }}
          onPointerDown={beginRange}
        >
          {segments.map((item) => {
            const from = Date.parse(item.started_at_utc)
            const to = Date.parse(item.ended_at_utc ?? item.started_at_utc)
            return (
              <button
                key={item.id}
                className={`segment ${item.id === selected ? 'selected' : ''} ${item.status}`}
                style={{ left: position(from), width: `${((to - from) / span) * 100}%` }}
                title={`${localClock(item.started_at_utc)} · ${duration(item.duration_ms)}`}
                onClick={() => {
                  if (swallowed.current) {
                    swallowed.current = false
                    return
                  }
                  onSelect(item.id)
                }}
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
          {/* The window is a wash over the whole height but does not take the
              pointer, so the segment blocks underneath stay clickable; only its
              two edges grab. */}
          {shown && (
            <div
              className="range"
              style={{
                left: position(shown.from),
                width: widthOf(shown.to - shown.from),
              }}
            >
              <span className="range-handle from" data-edge="from" title="调整起点" />
              <span className="range-handle to" data-edge="to" title="调整终点" />
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

/** Orders a dragged window; a drag to the left is still a window. */
function normalize(range: TimeRange): TimeRange {
  return range.from <= range.to ? range : { from: range.to, to: range.from }
}

/** Clock label for one timeline axis point; seconds when the window is short. */
function clockOf(time: number, span: number): string {
  const iso = new Date(time).toISOString()
  return span < 10 * 60 * 1000 ? localTime(iso) : localClock(iso)
}

/** Clock label for an arbitrary instant, always with seconds. */
function clockOfMs(ms: number): string {
  return localTime(new Date(ms).toISOString())
}

/** Whether an instant falls inside any of the sorted spans. */
function hasAudio(spans: { start: number; end: number }[], at: number): boolean {
  let low = 0
  let high = spans.length - 1
  while (low <= high) {
    const mid = (low + high) >> 1
    if (at < spans[mid].start) high = mid - 1
    else if (at >= spans[mid].end) low = mid + 1
    else return true
  }
  return false
}

/**
 * Joins phrases into passages by silence alone.
 *
 * Every phrase keeps its own identity — the row's jump target is the first one,
 * and the hover text carries the exact clock — so merging is a typographic
 * decision that never loses a timestamp.
 */
function buildPassages(phrases: Utterance[]): Passage[] {
  const passages: Passage[] = []
  for (const item of phrases) {
    const from = Date.parse(item.started_at_utc)
    const ended = Date.parse(item.ended_at_utc)
    // A malformed ended-at must not poison the merge arithmetic with NaN.
    const to = Number.isFinite(ended) ? Math.max(ended, from) : from
    const last = passages[passages.length - 1]
    if (
      last !== undefined &&
      from - last.to <= PASSAGE_GAP_MS &&
      to - last.from <= PASSAGE_MAX_MS &&
      last.phrases.length < PASSAGE_MAX_PHRASES
    ) {
      last.phrases.push(item)
      last.to = Math.max(last.to, to)
      last.spokenMs += to - from
      continue
    }
    passages.push({ id: item.id, phrases: [item], from, to, spokenMs: to - from })
  }
  return passages
}

/** Hover text: the exact clock, and how many phrases the row stands for. */
function passageTitle(passage: Passage): string {
  const first = passage.phrases[0]
  const last = passage.phrases[passage.phrases.length - 1]
  const span = `${localTime(first.started_at_utc)} — ${localTime(last.ended_at_utc)}`
  return passage.phrases.length > 1 ? `${span} · ${passage.phrases.length} 句` : span
}

function weekdayOf(date: string): string {
  const names = ['周日', '周一', '周二', '周三', '周四', '周五', '周六']
  return names[new Date(`${date}T12:00:00`).getDay()] ?? ''
}

function matchesDayFilter(day: Day, filter: DayFilter): boolean {
  if (filter === 'recorded') return day.segment_count > 0
  if (filter === 'events') return day.event_count > 0
  return true
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
    case 'deleted':
      return '已回收'
    default:
      return status
  }
}

/**
 * Whether a segment still has a file behind it. `deleted` is what retention
 * leaves behind — the row, the events and the transcript survive the audio — and
 * `missing` is a row whose file never appeared. Neither can be played, so the
 * player is not offered for them and a transcript jump skips past them.
 */
function playableStatus(status: string): boolean {
  return status !== 'deleted' && status !== 'missing'
}
