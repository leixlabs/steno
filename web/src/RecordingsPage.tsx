// The recordings page: one night, three stacked views.
//
//   1. 采集时间轴 — where the night has audio at all (recorded segments) and where
//      the classifier heard snoring. Zoom, pan and drag a window here; the window
//      narrows the stats and the transcript below.
//   2. 鼾声 — the snoring the classifier (or a reviewer) claimed: an overview
//      strip, the grouped episodes, and replay of each group.
//   3. 转录 — what was said, as a clock-stamped list; a row replays its audio.
//
// There is one <audio> element and no detail panel. Both replay paths (a snore
// group, a transcript row) drive it, and a slim "now playing" bar sits above the
// snoring panel and appears only once something has been started, so the page never shows a player with nothing
// behind it. Rows the rule detector alone produced are not drawn: they only mean
// "something was audible", which the segment blocks already say.
//
// Replay is a cursor over cues (see `buildCues`): an episode is a list of clips,
// each inside whatever segment held it, so moving the cursor may swap the
// player's `src`. A transcript row is a single seek.

import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  api,
  duration,
  localClock,
  localTime,
  share,
  shiftDate,
  todayISO,
  type Day,
  type Episode,
  type Recording,
  type Status,
  type Utterance,
} from './api'
import { Empty, PanelHead } from './ui'
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
 * A stretch of the night that was audible, with short breaks merged out.
 *
 * The detector reports *events*, one per snore run — thousands a night, most a
 * few seconds long. That is the right unit for the record and the wrong unit
 * for a reader: "21:03:07, 21:03:14, 21:03:22 …" is not a night, it is a log.
 * An episode is the smallest thing that answers the question the page is for —
 * "when was it loud, for how long, and can I hear it".
 *
 * `to - from` is the epoch span (how long the bout lasted, gaps included);
 * `soundMs` is the sum of the events themselves (how much of it was sound).
 * Both are shown because they mean different things, and their ratio is what
 * tells continuous snoring from a few isolated bursts.
 *
 * The same shape holds for both zones: a group of classified rows reads as a
 * bout of snoring, a group of rule candidates reads as a stretch with sound in
 * it. Only the labels differ.
 */
interface EpisodeGroup {
  /** The first event's id: stable across renders and unique on the page. */
  id: string
  events: Episode[]
  from: number
  to: number
  soundMs: number
  /** Events whose recording still has a file; the rest are counted, not playable. */
  playable: number
  /** Why these rows are in this zone, deduplicated: "分类器 · 未复核" and so on. */
  reasons: string[]
  /** One bar per bucket of `span`, as a fraction of that bucket — a shape, not a metric. */
  density: number[]
}

/** One clip of a replay: an event, and where it starts inside its recording. */
interface Cue {
  /** The row this clip came from: the detail panel follows the cursor. */
  event: Episode
  segmentId: string
  /** Offset of the event's start inside its own WAV file. */
  offsetMs: number
  /** Absolute end of the clip, clamped to the segment so a seek cannot run off it. */
  endsAt: number
}

/** A replay in progress: which episode, which clips, which one is sounding. */
interface Replay {
  episodeId: string
  cues: Cue[]
  index: number
}

/**
 * Which of the two lists a row belongs to.
 *
 * `snore` — the row claims *this was snoring*: the sidecar's SoundAnalysis lane
 * wrote it (`apple-soundanalysis-v1`, label `snoring` above its threshold), or
 * a reviewer confirmed it (`review_status = 'snore'`) — including a rule-v1 row
 * a human promoted — unless a reviewer excluded it.
 *
 * `sound` — everything else: rule-v1 rows nobody confirmed, and rows a reviewer
 * excluded (`not_snore`). The rule layer only measures band energy above the
 * noise floor, so it fires on speech, a television and a bed-time story just as
 * readily as on snoring; that is a map of "something was audible", not evidence
 * of snoring, and it must not stand in for an answer.
 *
 * `review_status` wins over `source` in the one case that matters: a classified
 * row a reviewer excluded moves to `sound` rather than being counted as snoring.
 */
type Zone = 'snore' | 'sound'

/** The classifier's name in `source`. */
const CLASSIFIER_PREFIX = 'apple-soundanalysis'

function zoneOf(event: Episode): Zone {
  // A reviewer's exclusion is the one label that can take a row out of the
  // snoring list, and it outranks the classifier that proposed it.
  if (event.review_status === 'not_snore') return 'sound'
  // A confirmation is equally decisive, even on a rule-v1 row.
  if (event.review_status === 'snore') return 'snore'
  return event.source.startsWith(CLASSIFIER_PREFIX) ? 'snore' : 'sound'
}

/** The readable name of an annotator. */
function sourceLabel(source: string): string {
  return source.startsWith(CLASSIFIER_PREFIX) ? '分类器' : '规则候选'
}

function reviewLabel(status: Episode['review_status']): string {
  switch (status) {
    case 'snore':
      return '已确认鼾声'
    case 'not_snore':
      return '已排除'
    default:
      return '未复核'
  }
}

/** Why a row is in the zone it is in, in the row's own words. */
function reasonOf(event: Episode): string {
  return `${sourceLabel(event.source)} · ${reviewLabel(event.review_status)}`
}

/**
 * Silence longer than this ends an episode.
 *
 * A view control, not a detector knob — the events in the database are
 * untouched, and the operator can change the threshold while looking at the
 * list (see `EPISODE_GAPS`). Two minutes is the default because a snorer's
 * bout keeps producing events every few seconds and a real break in the night
 * is measured in minutes: on a recorded night the same 1730 rule candidates
 * collapse to 72 episodes at 2 min, 116 at 1 min and 205 at 30 s, and the
 * snore total and record count are identical under all four thresholds.
 */
const DEFAULT_EPISODE_GAP_MS = 120_000
const EPISODE_GAPS = [
  { ms: 30_000, label: '30 秒' },
  { ms: 60_000, label: '1 分' },
  { ms: 120_000, label: '2 分' },
  { ms: 300_000, label: '5 分' },
] as const
/** Bars in an episode's density glyph. Enough for shape, few enough to stay crisp. */
const EPISODE_BUCKETS = 40
/**
 * Playback starts this far before an event. The detector opens an event when
 * band energy crosses the floor, which is a little after the audible onset;
 * seeking to the exact start clips the first breath.
 */
const PRE_ROLL_MS = 200

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

export default function RecordingsPage() {
  const [date, setDate] = useState(todayISO())
  const [status, setStatus] = useState<Status | null>(null)
  const [days, setDays] = useState<Day[]>([])
  const [segments, setSegments] = useState<Recording[]>([])
  const [events, setEvents] = useState<Episode[]>([])
  const [selectedSegment, setSelectedSegment] = useState<string | null>(null)
  const [utterances, setUtterances] = useState<Utterance[]>([])
  const [error, setError] = useState<string | null>(null)
  // The window picked on the timeline. It is view state, so it does not survive
  // a date change — a window on Tuesday means nothing on Wednesday.
  const [range, setRange] = useState<TimeRange | null>(null)
  /** Where the player should be after a jump; consumed by `onLoadedMetadata`. */
  const pendingSeek = useRef<{ segmentId: string; offsetMs: number } | null>(null)
  const [playhead, setPlayhead] = useState<number | null>(null)
  const [jumpNote, setJumpNote] = useState<string | null>(null)
  const audioRef = useRef<HTMLAudioElement>(null)
  /** How long a silence splits one episode into two. Shared by both zones. */
  const [episodeGap, setEpisodeGap] = useState(DEFAULT_EPISODE_GAP_MS)
  /** What the player is loaded for, in words; `null` until something was started. */
  const [nowPlaying, setNowPlaying] = useState<string | null>(null)
  const [hideFiller, setHideFiller] = useState(true)
  const [replay, setReplay] = useState<Replay | null>(null)
  const [replayNote, setReplayNote] = useState<string | null>(null)
  // The same replay, readable from event handlers that were captured in an
  // earlier render (`onTimeUpdate` fires off the media element's own clock, and
  // an advance must not act on a stale index).
  const replayRef = useRef<Replay | null>(null)

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
    // A replay belongs to the day that is being left, and its cues point at
    // segments this page no longer holds.
    replayRef.current = null
    setReplay(null)
    setReplayNote(null)
    setNowPlaying(null)
    Promise.all([api.recordings(date), api.events(date), api.utterances(date)])
      .then(([nextSegments, nextEvents, nextUtterances]) => {
        if (cancelled) return
        setSegments(nextSegments)
        setEvents(nextEvents)
        setUtterances(nextUtterances)
        // Open on the first recording that actually has audio. A `recordings`
        // row only exists once its file was written, so the only empty case left
        // is a zero-length take.
        const firstWithAudio =
          nextSegments.find((item) => item.duration_ms > 0) ?? nextSegments[0]
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
  // Which stretches of the night have audio behind them. A `recordings` row is
  // written when its file closes, so every row here is a file that exists;
  // merged, so a lookup never has to assume disjoint spans.
  const playableSpans = useMemo(() => {
    const spans = segments
      .map((item) => {
        const start = Date.parse(item.started_at_utc)
        const end = Date.parse(item.ended_at_utc) || start + item.duration_ms
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

  const snoreEvents = useMemo(() => events.filter((event) => zoneOf(event) === 'snore'), [events])

  /**
   * The day's snoring, aggregated. Rebuilt when the threshold changes because
   * the threshold *is* the aggregation: there is no canonical episode list to
   * cache, only this view's answer to "how close must two bouts be to be one".
   */
  const snoreGroups = useMemo(
    () => buildEpisodes(events, episodeGap, segments, 'snore'),
    [events, episodeGap, segments],
  )
  const snoreTotals = useMemo(() => totalsOf(snoreGroups), [snoreGroups])
  /** First and last record: the span the overview strip is drawn over. */
  const snoreNight = useMemo(() => nightOf(snoreGroups), [snoreGroups])

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
      const end = Date.parse(item.ended_at_utc) || start + item.duration_ms
      const inside = clip(start, end)
      if (inside > 0) {
        segmentCount += 1
        recordedMs += inside
      }
    }

    // The same partition the list uses, or the head would count rows the
    // panel below it does not show.
    let snoreCount = 0
    for (const event of events) {
      const at = Date.parse(event.started_at_utc)
      if (at < from || at > to) continue
      if (zoneOf(event) === 'snore') snoreCount += 1
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

    return {
      from,
      to,
      ms: to - from,
      segmentCount,
      recordedMs,
      snoreCount,
      utteranceCount,
      spokenMs: spoken,
    }
  }, [range, segments, events, allUtterances])

  /**
   * A transcript is built from phrases but read in passages. The recogniser
   * emits a final every few seconds, so one remark arrives as several rows of
   * two or three characters — "我吗 / 我也 / 嗯。" is complete and unreadable.
   * See `buildPassages` for the rule.
   */
  const passages = useMemo(() => buildPassages(allUtterances), [allUtterances])
  // A recogniser given a quiet room answers "嗯。" / "哦。" over and over. They are
  // real rows in the database and stay there; the toggle only decides whether the
  // list is made of them. Judged per passage, so a sentence that merely *contains*
  // a filler word is never hidden.
  const fillerCount = useMemo(() => passages.filter(isFillerPassage).length, [passages])
  const shownPassages = useMemo(
    () => (hideFiller ? passages.filter((passage) => !isFillerPassage(passage)) : passages),
    [passages, hideFiller],
  )

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
   * Jumps to the audio behind one sentence: selects the recording that holds it
   * and seeks the player to the offset. A phrase can fall outside every
   * recording — the transcript is not obligatorily backed by audio — and "no
   * recording" is a real answer, said out loud rather than silently doing
   * nothing.
   */
  const jumpTo = (item: Utterance) => {
    const at = Date.parse(item.started_at_utc)
    const host = segments.find((candidate) => {
      const start = Date.parse(candidate.started_at_utc)
      const end = Date.parse(candidate.ended_at_utc) || start + candidate.duration_ms
      return at >= start && at < end
    })
    if (!host) {
      setJumpNote('这一句的时刻不在当天任何一段录音里；转写与鼾声标注仍然保留。')
      return
    }
    const offsetMs = Math.max(at - Date.parse(host.started_at_utc), 0)
    setJumpNote(null)
    // A sentence is a new destination; a half-finished episode replay would
    // steer the player back to its own cue list, and the panel stops explaining
    // a record it is no longer playing.
    if (replayRef.current) stopReplay(null)
    setNowPlaying(`转录 ${localClock(item.started_at_utc)}`)
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
    const at = Date.parse(segment.started_at_utc) + player.currentTime * 1000
    setPlayhead(at)
    const state = replayRef.current
    // A pending seek means the element is still loaded with the previous src, so
    // `at` is measured from the wrong origin and must not decide anything.
    if (!state || pendingSeek.current) return
    const cue = state.cues[state.index]
    if (!cue || cue.segmentId !== segment.id) return
    if (at >= cue.endsAt) advance(state)
  }

  /**
   * The file ended under a cue that had not finished — a clip that runs to the
   * end of its segment never gets a `timeupdate` past `endsAt`.
   *
   * The segment check is also the guard against advancing twice: `onTimeUpdate`
   * moves the cursor to the next cue (usually in the next segment), so a late
   * `ended` from the file we just left finds a cursor that no longer belongs to
   * it and does nothing.
   */
  const onSegmentEnded = () => {
    const state = replayRef.current
    if (!state || !segment) return
    const cue = state.cues[state.index]
    if (!cue || cue.segmentId !== segment.id) return
    advance(state)
  }

  // --- replay ---------------------------------------------------------------
  //
  // An episode is not a file: it is a list of clips, each inside whatever
  // segment happened to hold it. Playback is therefore a cursor over cues, and
  // moving the cursor may also have to swap the player's `src`. Both paths end
  // in `seekToCue`, so exactly one place knows how a cue becomes a seek.

  /** Commits a replay to the ref (for media callbacks) and the state (for render). */
  const commitReplay = (next: Replay | null) => {
    replayRef.current = next
    setReplay(next)
  }

  const seekToCue = (cue: Cue) => {
    const offsetMs = Math.max(cue.offsetMs - PRE_ROLL_MS, 0)
    const player = audioRef.current
    // Same src and loaded enough to seek: no round trip through React needed.
    if (player && segment?.id === cue.segmentId && player.readyState >= 1) {
      player.currentTime = offsetMs / 1000
      void player.play().catch(() => undefined)
      return
    }
    setSelectedSegment(cue.segmentId)
    // A seek before `loadedmetadata` is dropped; hand the offset to the
    // element's own event instead of guessing a timeout.
    pendingSeek.current = { segmentId: cue.segmentId, offsetMs }
  }

  const advance = (state: Replay) => {
    const next = state.index + 1
    if (next >= state.cues.length) {
      audioRef.current?.pause()
      commitReplay(null)
      setReplayNote(`这一段的 ${state.cues.length} 条记录已经放完了。`)
      return
    }
    commitReplay({ ...state, index: next })
    seekToCue(state.cues[next])
  }

  /**
   * Starts an episode. The window it covers becomes the page's selection, so
   * the timeline, the head stats and the transcript all narrow to the same
   * stretch the audio is coming from.
   */
  const playEpisode = (group: EpisodeGroup) => {
    setRange({ from: group.from, to: group.to })
    const { cues, dropped } = buildCues(group, segments)
    if (cues.length === 0) {
      commitReplay(null)
      // Nothing to seek to: the `recording_id = null` case, said out loud.
      setReplayNote('这一段的录音都不在本机了（容量回收）；标注仍然保留。')
      return
    }
    setReplayNote(dropped > 0 ? `${dropped} 条记录所在的录音已被回收，回放会跳过它们。` : null)
    const state: Replay = { episodeId: group.id, cues, index: 0 }
    setNowPlaying(`鼾声 ${clockOfMs(group.from)} – ${clockOfMs(group.to)}`)
    commitReplay(state)
    seekToCue(cues[0])
  }

  const stopReplay = (note: string | null = null) => {
    audioRef.current?.pause()
    commitReplay(null)
    setReplayNote(note)
  }

  const toggleEpisode = (group: EpisodeGroup) => {
    if (replayRef.current?.episodeId === group.id) {
      stopReplay(null)
      return
    }
    playEpisode(group)
  }

  /** Selecting a recording by hand ends the replay, or its cursor would drag the player back. */
  const selectSegment = (id: string) => {
    if (replayRef.current) stopReplay(null)
    const picked = segments.find((item) => item.id === id)
    if (picked) setNowPlaying(`录音 ${localClock(picked.started_at_utc)} 起`)
    setSelectedSegment(id)
  }

  const chooseGap = (ms: number) => {
    stopReplay(null)
    setEpisodeGap(ms)
  }

  // A stable identity for the episode table's click handler. The tables are the
  // big subtrees on this page and `timeupdate` re-renders the page four times a
  // second; without this the handler would be a new function every tick and
  // every row would reconcile with it.
  const pickRef = useRef<(group: EpisodeGroup) => void>(() => undefined)
  pickRef.current = toggleEpisode
  const onEpisodePick = useCallback((group: EpisodeGroup) => pickRef.current(group), [])

  // The day strip: the busiest day of the window sets the scale for every bar,
  // so a chip's bar means the same thing across the whole strip.
  const peakRecordedMs = Math.max(1, ...days.map((item) => item.recorded_ms))

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
          {days.map((item) => (
            <button
              key={item.date}
              className={[
                'day-chip',
                item.date === date ? 'active' : '',
                item.date === todayISO() ? 'today' : '',
                item.snore_count > 0 ? 'has-events' : '',
              ]
                .filter(Boolean)
                .join(' ')}
              onClick={() => setDate(item.date)}
              title={[
                weekdayOf(item.date),
                `${item.recording_count} 段录音`,
                duration(item.recorded_ms),
                item.snore_count ? `${item.snore_count} 条鼾声` : '',
                item.lost_frames ? `丢帧 ${item.lost_frames.toLocaleString()}` : '',
              ]
                .filter(Boolean)
                .join(' · ')}
            >
              <span className="day-date">{item.date.slice(5)}</span>
              <span className="day-count">{item.recording_count > 0 ? item.recording_count : '—'}</span>
              {/* A bar, not a number: a month of nights is compared by shape, and
                  the count alone cannot tell a 20-minute night from an 8-hour one. */}
              <span className="day-bar" aria-hidden>
                <i style={{ width: `${share(item.recorded_ms, peakRecordedMs) * 100}%` }} />
              </span>
            </button>
          ))}
          {days.length === 0 && <span className="day-empty">这 30 天里没有日期</span>}
        </div>

      </div>

      {/* 1 — where the night has sound. */}
      <section className="panel timeline-panel">
        <div className="panel-head timeline-head">
          <h2>采集时间轴</h2>
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
                鼾声 <b>{rangeStats.snoreCount}</b>
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
          events={snoreEvents}
          selected={selectedSegment}
          onSelect={selectSegment}
          range={range}
          onRange={setRange}
        />
      </section>

    {/* Mounted from the start so a click can seek it, but only shown once
          something has been started: a player with nothing behind it reads as
          broken. */}
      <div className={`now-playing ${nowPlaying && segment ? '' : 'idle'}`}>
        {nowPlaying && <span className="now-label">{nowPlaying}</span>}
        <audio
          ref={audioRef}
          controls
          preload="metadata"
          src={segment?.audio_url}
          className="player"
          onLoadedMetadata={onLoadedMetadata}
          onTimeUpdate={onTimeUpdate}
          onSeeked={onTimeUpdate}
          onEnded={onSegmentEnded}
          onError={() => {
            if (replayRef.current) stopReplay('这一段音频读不出来，可能文件已缺失。')
          }}
        />
      </div>

      {/* 2 — the snoring itself. */}
      <section className="panel snore-panel">
        <div className="panel-head snore-head">
          <h2>鼾声</h2>
          <div className="head-controls">
            <span>
              {snoreTotals.groups
                ? `${snoreTotals.groups} 段 · 共 ${duration(snoreTotals.soundMs)} · ${snoreTotals.count} 条`
                : '这一天没有鼾声'}
            </span>
            {/* The threshold is a lens, not a setting: it re-groups the same
                records, it never writes anything back. */}
            <div className="gap-control" role="group" aria-label="合并间隔">
              <span>合并间隔</span>
              {EPISODE_GAPS.map((item) => (
                <button
                  key={item.ms}
                  className={item.ms === episodeGap ? 'on' : ''}
                  onClick={() => chooseGap(item.ms)}
                  aria-pressed={item.ms === episodeGap}
                  title={`相隔不到 ${item.label} 的记录就算同一段`}
                >
                  {item.label}
                </button>
              ))}
            </div>
          </div>
        </div>
        {snoreGroups.length === 0 ? (
          <Empty text="今天没有分类器判定的鼾声。" />
        ) : (
          <>
            {snoreNight && (
              <ZoneStrip
                groups={snoreGroups}
                night={snoreNight}
                activeId={replay?.episodeId ?? null}
                onPick={onEpisodePick}
              />
            )}
            <EpisodeTable
              groups={snoreGroups}
              activeId={replay?.episodeId ?? null}
              onPick={onEpisodePick}
            />
          </>
        )}
        {replayNote && <p className="snore-note">{replayNote}</p>}
        {snoreGroups.length > 0 && (
          <p className="snore-foot">
            相邻的记录按合并间隔拼成一段。点一行逐条回放，中间的安静会跳过。
            {snoreTotals.playable < snoreTotals.count &&
              `其中 ${snoreTotals.count - snoreTotals.playable} 条所在的录音已被容量回收。`}
          </p>
        )}
      </section>

      {/* 3 — what was said. Clicking a row plays it in the bar above. */}
      <section className="panel utterances-panel">
        <PanelHead
          title={`${date} 转录`}
          meta={
            status?.stt.enabled
              ? `${status.stt.engine} · ${allUtterances.length} 句 · ${duration(spokenMs)}`
              : `${allUtterances.length} 句 · 转写未开启`
          }
        />
        <div className="transcript-tools">
          <label className="filler-toggle">
            <input
              type="checkbox"
              checked={hideFiller}
              onChange={(event) => setHideFiller(event.target.checked)}
            />
            折叠语气词
            {fillerCount > 0 && <i>{fillerCount} 条</i>}
          </label>
        </div>
        {allUtterances.length === 0 && !(isToday && live.partial) && (
          <Empty text={transcriptEmptyHint(status)} />
        )}
        {shownPassages.length > 0 && (
          <ol className="transcript">
            {shownPassages.map((passage) => {
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
                      reachable ? '点击回放这一段' : '这一段录音已被回收'
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
            {duration(spokenMs)}
            {hideFiller && fillerCount > 0 && `；已折叠 ${fillerCount} 条只有语气词的短句`}
            ；点击任一条回放它开头的录音，播放时会跟随高亮。
          </p>
        )}
      </section>
    </>
  )
}

// --- episodes ---------------------------------------------------------------

/**
 * The day's groups of one zone on one line, before the table.
 *
 * The timeline answers "what was recorded"; this answers "when was it loud",
 * and it is also the fastest way into a group — every block is a button. Two
 * strips exist, one per zone, so each keeps its own clock range and a block's
 * position is read against its own zone's first and last record.
 */
function ZoneStrip({
  groups,
  night,
  activeId,
  onPick,
}: {
  groups: EpisodeGroup[]
  night: { from: number; to: number; span: number }
  activeId: string | null
  onPick: (group: EpisodeGroup) => void
}) {
  return (
    <div className="snore-strip">
      <div className="snore-strip-track">
        {groups.map((group) => (
          <button
            key={group.id}
            className={[
              'snore-block',
              activeId === group.id ? 'active' : '',
              group.playable === 0 ? 'gone' : '',
            ]
              .filter(Boolean)
              .join(' ')}
            style={{
              left: `${((group.from - night.from) / night.span) * 100}%`,
              width: `${Math.max(((group.to - group.from) / night.span) * 100, 0.4)}%`,
            }}
            onClick={() => onPick(group)}
            aria-label={`${clockOfMs(group.from)} 到 ${clockOfMs(group.to)}`}
            title={`${clockOfMs(group.from)} – ${clockOfMs(group.to)} · ${
              group.events.length
            } 条`}
          />
        ))}
      </div>
      <div className="snore-strip-axis">
        <span>{clockOfMs(night.from)}</span>
        <span>{clockOfMs(night.to)}</span>
      </div>
    </div>
  )
}

/**
 * The snoring table, memoised.
 *
 * Playing a night re-renders this page about four times a second, because the
 * player's `timeupdate` drives the transcript highlight — and each table is
 * dozens of rows of forty density bars. Only `activeId` actually changes
 * between those renders, so the tables are cut out of that path: same props in,
 * nothing reconciled. `onPick` is stabilised by the caller for the same reason.
 */
const EpisodeTable = memo(function EpisodeTable({
  groups,
  activeId,
  onPick,
}: {
  groups: EpisodeGroup[]
  activeId: string | null
  onPick: (group: EpisodeGroup) => void
}) {
  const listRef = useRef<HTMLOListElement>(null)
  // A group can be started from the overview strip above, where the matching
  // row is usually scrolled out of a long list. Following the active row is the
  // difference between "the strip did something" and "which one am I hearing".
  // Only on change, so it never fights the reader's own scrolling.
  useEffect(() => {
    if (!activeId) return
    listRef.current
      ?.querySelector(`[data-episode="${activeId}"]`)
      ?.scrollIntoView({ block: 'nearest' })
  }, [activeId])

  if (groups.length === 0) return null
  const noun = '鼾声'
  return (
    <>
      <div className="episode-cols" aria-hidden>
        <span>时段</span>
        <span>{noun}密度</span>
        <span className="right">{noun}时长</span>
        <span className="right">记录</span>
        <span>来源</span>
        <span />
      </div>
      <ol className="episode-list" ref={listRef}>
        {groups.map((group) => {
          const active = activeId === group.id
          const span = Math.max(group.to - group.from, 1)
          const ratio = Math.round((group.soundMs / span) * 100)
          // Normalised per group: the glyph is a shape, the numbers beside it
          // are the measurement.
          const peak = Math.max(...group.density, 0.000001)
          const gone = group.events.length - group.playable
          return (
            <li key={group.id}>
              <button
                data-episode={group.id}
                className={[
                  'episode-row',
                  active ? 'active' : '',
                  group.playable === 0 ? 'no-audio' : '',
                ]
                  .filter(Boolean)
                  .join(' ')}
                onClick={() => onPick(group)}
                aria-pressed={active}
                title={[
                  `${clockOfMs(group.from)} – ${clockOfMs(group.to)}`,
                  `持续 ${duration(span)}`,
                  `${noun} ${duration(group.soundMs)}（占 ${ratio}%）`,
                  `${group.events.length} 条记录`,
                  group.reasons.join(' / '),
                  gone > 0 ? `${gone} 条录音已回收` : '',
                  active ? '点击停止' : `点击回放这几条${noun}`,
                ]
                  .filter(Boolean)
                  .join(' · ')}
              >
                <span className="episode-span">
                  <b>{clockOfMs(group.from)}</b>
                  <em>–</em>
                  <b>{clockOfMs(group.to)}</b>
                  <i>持续 {duration(span)}</i>
                </span>
                <span
                  className={`episode-density ${group.density.length === 0 ? 'single' : ''}`}
                  aria-hidden
                >
                  {group.density.map((value, index) => (
                    <i
                      key={index}
                      style={{
                        height: `${value > 0 ? Math.max((value / peak) * 100, 8) : 2}%`,
                        opacity: value > 0 ? 1 : 0.35,
                      }}
                    />
                  ))}
                </span>
                <span className="episode-stat">
                  <b>{duration(group.soundMs)}</b>
                  <i>{ratio}%</i>
                </span>
                <span className="episode-count">
                  <b>{group.events.length}</b> 条
                </span>
                {/* Who wrote the row and whether a reviewer touched it. */}
                <span className="episode-reason">{group.reasons.join(' / ')}</span>
                <span className="episode-play" aria-hidden>
                  {active ? '■' : '▶'}
                </span>
              </button>
            </li>
          )
        })}
      </ol>
    </>
  )
})

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
  segments: Recording[]
  events: Episode[]
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
    ...segments.map((item) => Date.parse(item.ended_at_utc) || Date.now()),
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

      <div className="timeline-legend">
        <span className="legend-item segment-key">
          <i />
          有录音
        </span>
        <span className="legend-item snore">
          <i />
          鼾声
        </span>
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
            const to = Date.parse(item.ended_at_utc)
            return (
              <button
                key={item.id}
                className={`segment ${item.id === selected ? 'selected' : ''}`}
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
              className={['tick', zoneOf(event), event.review_status]
                .filter(Boolean)
                .join(' ')}
              style={{ left: position(Date.parse(event.started_at_utc)) }}
              title={`${reasonOf(event)} · ${localTime(event.started_at_utc)} · ${(
                event.duration_ms / 1000
              ).toFixed(1)}s`}
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

/**
 * Groups one zone's events into episodes.
 *
 * The rule is one number: two events belong to the same episode when the gap
 * between them is no wider than `gapMs`. Everything else — the span, the sound
 * total, the density glyph, whether an episode is still audible — is derived
 * from the events inside, so the aggregation cannot disagree with the records
 * it is made of.
 *
 * `zone` decides *which* events count, and it is honoured strictly: the two
 * zones partition the day (`zoneOf`), so no row is counted twice and none is
 * dropped. A row whose audio was reclaimed (`recording_id = null`) is still
 * counted and still shown — it just cannot be played.
 */
function buildEpisodes(
  events: Episode[],
  gapMs: number,
  segments: Recording[],
  zone: Zone,
): EpisodeGroup[] {
  // A `recordings` row only exists once its file was written, so every id here
  // names audio that can be served.
  const audible = new Set(segments.map((item) => item.id))
  const points = events
    .filter((event) => zoneOf(event) === zone)
    .map((event) => {
      const from = Date.parse(event.started_at_utc)
      const ended = Date.parse(event.ended_at_utc)
      // A malformed ended-at must not poison the merge arithmetic with NaN.
      const to = Number.isFinite(ended) ? Math.max(ended, from) : from + event.duration_ms
      return { event, from, to }
    })
    .filter((item) => Number.isFinite(item.from))
    .sort((a, b) => a.from - b.from)

  const spans: { events: Episode[]; from: number; to: number }[] = []
  for (const point of points) {
    const current = spans[spans.length - 1]
    if (current && point.from - current.to <= gapMs) {
      current.events.push(point.event)
      // Overlapping events exist (a long run split in two), so the end is a max.
      current.to = Math.max(current.to, point.to)
      continue
    }
    spans.push({ events: [point.event], from: point.from, to: point.to })
  }

  return spans.map((span) => {
    let soundMs = 0
    let playable = 0
    const reasons = new Set<string>()
    for (const event of span.events) {
      soundMs += event.duration_ms
      reasons.add(reasonOf(event))
      if (event.recording_id !== null && audible.has(event.recording_id)) playable += 1
    }
    return {
      id: span.events[0].id,
      events: span.events,
      from: span.from,
      to: span.to,
      soundMs,
      playable,
      reasons: [...reasons],
      // A single record is a point, not a distribution — see `.episode-density`.
      density: span.events.length > 1 ? densityOf(span.events, span.from, span.to) : [],
    }
  })
}

/** Totals for a zone's head: how many groups, how long, how many records. */
function totalsOf(groups: EpisodeGroup[]): {
  groups: number
  soundMs: number
  count: number
  playable: number
} {
  let soundMs = 0
  let count = 0
  let playable = 0
  for (const group of groups) {
    soundMs += group.soundMs
    count += group.events.length
    playable += group.playable
  }
  return { groups: groups.length, soundMs, count, playable }
}

/** The span a zone's overview strip is drawn over: its own first and last record. */
function nightOf(groups: EpisodeGroup[]): { from: number; to: number; span: number } | null {
  if (groups.length === 0) return null
  const from = groups[0].from
  const to = groups[groups.length - 1].to
  return { from, to, span: Math.max(to - from, 1) }
}

/**
 * How much of each equal slice of the episode was snoring, as a fraction.
 *
 * A fraction rather than a count because episodes differ in length by two
 * orders of magnitude: 40 events in 30 seconds and 40 events in 40 minutes are
 * very different nights, and only the fraction makes them comparable.
 */
function densityOf(events: Episode[], from: number, to: number): number[] {
  const span = Math.max(to - from, 1)
  const slice = span / EPISODE_BUCKETS
  const buckets = new Array<number>(EPISODE_BUCKETS).fill(0)
  for (const event of events) {
    const start = Math.max(Date.parse(event.started_at_utc) - from, 0)
    const end = Math.min(Date.parse(event.ended_at_utc) - from, span)
    if (!(end > start)) continue
    const first = Math.min(Math.floor(start / slice), EPISODE_BUCKETS - 1)
    const last = Math.min(Math.floor(end / slice), EPISODE_BUCKETS - 1)
    for (let index = first; index <= last; index += 1) {
      const low = index * slice
      const high = low + slice
      buckets[index] += Math.max(Math.min(end, high) - Math.max(start, low), 0)
    }
  }
  return buckets.map((value) => value / slice)
}

/**
 * Turns an episode into playable clips.
 *
 * The event names its recording but not the offset inside the file, and that
 * recording may be unknown (`recording_id = null`, audio reclaimed): both are
 * resolved here, in one place, so playback never has to reason about files. An
 * event with no recording is dropped rather than attributed to a neighbouring
 * file — guessing a host from the timestamps would invent a fact the row does
 * not carry.
 */
function buildCues(
  group: EpisodeGroup,
  segments: Recording[],
): { cues: Cue[]; dropped: number } {
  const byId = new Map(segments.map((item) => [item.id, item]))
  const cues: Cue[] = []
  let dropped = 0
  for (const event of group.events) {
    const host = event.recording_id === null ? undefined : byId.get(event.recording_id)
    if (!host) {
      dropped += 1
      continue
    }
    const start = Date.parse(host.started_at_utc)
    const end = Date.parse(host.ended_at_utc) || start + host.duration_ms
    const cueStart = Date.parse(event.started_at_utc)
    const cueEnd = Date.parse(event.ended_at_utc)
    if (cueStart >= end || cueEnd <= start) {
      // The event names a recording that does not contain it: a corrupt row is
      // not something to seek to.
      dropped += 1
      continue
    }
    cues.push({
      event,
      segmentId: host.id,
      offsetMs: Math.max(cueStart - start, 0),
      endsAt: Math.min(Number.isFinite(cueEnd) ? cueEnd : cueStart + event.duration_ms, end),
    })
  }
  return { cues, dropped }
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

/** Words that carry no content when they are all a passage says. */
const FILLER = new Set(['嗯', '哦', '啊', '呃', '额', '唉', '哎', '哈', '噢', '喔', '是', '好', '对', '呀', '哼', '诶'])

/** True when every character of the passage is a filler word or punctuation. */
function isFillerPassage(passage: Passage): boolean {
  const text = passage.phrases.map((item) => item.text).join('')
  const letters = [...text].filter((char) => !/[\s。，、！？,.!?…~～]/.test(char))
  return letters.length > 0 && letters.every((char) => FILLER.has(char))
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
