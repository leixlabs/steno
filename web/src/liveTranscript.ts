// Live transcript over WebSocket.
//
// The recogniser emits a partial several times a second while someone talks and a
// final when a phrase is done. Polling `/status` every 4 s showed the live line
// stuttering and, worse, could miss a short phrase entirely: spoken and
// finalised between two polls meant the transcript skipped it until the page was
// reloaded. This subscribes instead.
//
// Contract, and why each part is here:
//
// - `ready` arrives first. It carries the engine name and the phrase count at
//   connect time, so the page knows what it *did not* receive: everything before
//   this moment is history and comes from `/api/v1/utterances`.
// - `final` is sent by the server *after* the row is written, so a phrase shown
//   here is a phrase that exists on disk.
// - `lagged` means the browser fell behind and events were dropped. The page says
//   so instead of rendering a transcript with a silent hole.
// - The socket reconnects on its own with backoff, and stops when the component
//   unmounts or when the page is hidden (a phone in a pocket should not hold a
//   socket open).

import { useEffect, useRef, useState } from 'react'
import type { Utterance } from './api'

/** Where the socket lives. Same origin as the page, so no CORS and no config. */
function transcriptUrl(): string {
  const protocol = window.location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${protocol}//${window.location.host}/api/v1/transcript`
}

type ServerEvent =
  | { type: 'ready'; engine: string; finals_total: number }
  | { type: 'partial'; text: string }
  | {
      type: 'final'
      id: string
      started_at_utc: string
      ended_at_utc: string
      text: string
      engine: string
      language: string | null
    }
  | { type: 'state'; state: string }
  | { type: 'error'; message: string }
  | { type: 'lagged'; missed: number }

export type StreamStatus = 'connecting' | 'live' | 'reconnecting' | 'closed'

export interface LiveTranscript {
  status: StreamStatus
  /** The half-sentence being formed, or null. */
  partial: string | null
  /** Phrases that arrived while this page was open, oldest first. */
  finals: Utterance[]
  /** Total phrases the server had stored when the socket opened. */
  historyAtConnect: number | null
  /** Set when the server said this browser missed events. */
  lagged: number
  /** Last failure, for the panel to show instead of an empty transcript. */
  error: string | null
  engine: string | null
}

const INITIAL: LiveTranscript = {
  status: 'connecting',
  partial: null,
  finals: [],
  historyAtConnect: null,
  lagged: 0,
  error: null,
  engine: null,
}

/**
 * Subscribes to the live transcript.
 *
 * `enabled` is what keeps this honest on a date that is not today: the stream is
 * always *now*, so appending it to a page showing last Tuesday would be a lie.
 */
export function useLiveTranscript(enabled: boolean): LiveTranscript {
  const [state, setState] = useState<LiveTranscript>(INITIAL)
  const socketRef = useRef<WebSocket | null>(null)
  const retryRef = useRef(0)
  const timerRef = useRef<number | null>(null)

  useEffect(() => {
    if (!enabled) {
      setState(INITIAL)
      return
    }

    let disposed = false

    const connect = () => {
      if (disposed) return
      let socket: WebSocket
      try {
        socket = new WebSocket(transcriptUrl())
      } catch (cause) {
        setState((prev) => ({ ...prev, status: 'reconnecting', error: (cause as Error).message }))
        schedule()
        return
      }
      socketRef.current = socket

      socket.onopen = () => {
        retryRef.current = 0
        setState((prev) => ({ ...prev, status: 'live', error: null }))
      }

      socket.onmessage = (message) => {
        let event: ServerEvent
        try {
          event = JSON.parse(message.data as string) as ServerEvent
        } catch {
          // A frame we cannot parse is not worth crashing the panel over.
          return
        }
        setState((prev) => apply(prev, event))
      }

      socket.onerror = () => {
        // `onclose` always follows; nothing to do but let it handle the retry.
      }

      socket.onclose = () => {
        socketRef.current = null
        if (disposed) return
        setState((prev) => ({ ...prev, status: 'reconnecting' }))
        schedule()
      }
    }

    const schedule = () => {
      if (disposed) return
      // 1 s, 2 s, 4 s, capped at 10 s: a server restart is a few seconds, a
      // crashed server should not be hammered.
      const delay = Math.min(1000 * 2 ** retryRef.current, 10_000)
      retryRef.current += 1
      timerRef.current = window.setTimeout(connect, delay)
    }

    // A hidden tab does not need the stream. Closing it also stops the server
    // counting a subscriber that nobody is watching.
    const onVisibility = () => {
      if (document.visibilityState === 'hidden') {
        timerRef.current && window.clearTimeout(timerRef.current)
        socketRef.current?.close()
        socketRef.current = null
        setState((prev) => ({ ...prev, status: 'closed' }))
      } else if (!socketRef.current && !disposed) {
        connect()
      }
    }

    connect()
    document.addEventListener('visibilitychange', onVisibility)

    return () => {
      disposed = true
      document.removeEventListener('visibilitychange', onVisibility)
      if (timerRef.current) window.clearTimeout(timerRef.current)
      socketRef.current?.close()
      socketRef.current = null
    }
  }, [enabled])

  return state
}

/** Folds one server event into the client state. */
function apply(prev: LiveTranscript, event: ServerEvent): LiveTranscript {
  switch (event.type) {
    case 'ready':
      return {
        ...prev,
        status: 'live',
        engine: event.engine,
        historyAtConnect: event.finals_total,
        error: null,
      }
    case 'partial':
      return { ...prev, partial: event.text.trim() === '' ? null : event.text }
    case 'final': {
      // The partial that produced this phrase is spent. Leaving it would show the
      // sentence twice until the next partial arrives.
      const phrase: Utterance = {
        id: event.id,
        started_at_utc: event.started_at_utc,
        ended_at_utc: event.ended_at_utc,
        engine: event.engine,
        language: event.language,
        text: event.text,
      }
      // Dedupe by id: a reconnect re-sends `ready`, and the page may already have
      // this row from its initial HTTP fetch.
      if (prev.finals.some((item) => item.id === phrase.id)) {
        return { ...prev, partial: null }
      }
      return { ...prev, partial: null, finals: [...prev.finals, phrase] }
    }
    case 'state':
      // The recognise came up after this page connected: nothing to add, but the
      // status line should stop claiming it is connecting.
      return event.state === 'ready' ? { ...prev, status: 'live' } : prev
    case 'error':
      return { ...prev, error: event.message, partial: null }
    case 'lagged':
      return { ...prev, lagged: prev.lagged + event.missed }
  }
}
