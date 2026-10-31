// The shell: one brand bar, one status chip, and a switch between the two pages.
//
// Routing is a hash rather than a path on purpose. The server serves this SPA
// from a directory and falls back to index.html for unknown paths, so a path
// route would work too — but `#/console` also survives being opened from a
// file:// preview and needs no server cooperation at all. Two pages do not
// justify a router dependency.
//
// Recordings is the landing page: the night is what this device is for, and the
// console is the answer to a question you only ask when something looks wrong.

import { useEffect, useState } from 'react'
import { api, type Status } from './api'
import ConsolePage from './ConsolePage'
import RecordingsPage from './RecordingsPage'
import { serviceLabel, serviceTone, toneClass } from './ui'

type Page = 'console' | 'recordings'

const PAGES: { id: Page; label: string; hint: string }[] = [
  { id: 'recordings', label: '录音', hint: '时间轴 · 片段 · 语句' },
  { id: 'console', label: '控制台', hint: '服务端 · 采集端 · 存储' },
]

function pageFromHash(): Page {
  return window.location.hash.replace(/^#\/?/, '') === 'console' ? 'console' : 'recordings'
}

export default function App() {
  const [page, setPage] = useState<Page>(pageFromHash)
  const [status, setStatus] = useState<Status | null>(null)

  useEffect(() => {
    const onHash = () => setPage(pageFromHash())
    window.addEventListener('hashchange', onHash)
    return () => window.removeEventListener('hashchange', onHash)
  }, [])

  // The header chip belongs to the shell, not to a page: whichever page you are
  // on, "is the server answering" is worth one glance. The console polls it for
  // its own cards; this poll only feeds the chip, so it is deliberately slow.
  useEffect(() => {
    let cancelled = false
    const tick = () =>
      api
        .status()
        .then((next) => !cancelled && setStatus(next))
        .catch(() => !cancelled && setStatus(null))
    void tick()
    const timer = setInterval(tick, 8000)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [])

  const go = (next: Page) => {
    window.location.hash = `#/${next}`
    setPage(next)
  }

  return (
    <div className="app">
      <header className="topbar">
        <div className="brand">
          <span className="brand-mark" aria-hidden />
          <div>
            <h1>steno</h1>
            <p>{PAGES.find((item) => item.id === page)?.hint}</p>
          </div>
        </div>

        <nav className="tabs" aria-label="页面">
          {PAGES.map((item) => (
            <button
              key={item.id}
              className={item.id === page ? 'active' : ''}
              onClick={() => go(item.id)}
              aria-current={item.id === page ? 'page' : undefined}
            >
              {item.label}
            </button>
          ))}
        </nav>

        <div className="topbar-meta">
          <span className={`chip ${status ? toneClass(serviceTone(status)) : 'warn'}`}>
            <i /> {status ? serviceLabel(status) : '服务端不可达'}
          </span>
          {status && <span className="chip muted">v{status.service.version}</span>}
          {status?.service.last_error && (
            <span className="chip warn" title={status.service.last_error}>
              <i /> 最近错误
            </span>
          )}
        </div>
      </header>

      {page === 'console' ? <ConsolePage /> : <RecordingsPage />}
    </div>
  )
}
