import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { Agentation } from 'agentation'
import App from './App'
import './styles.css'

/**
 * The annotation toolbar is a development tool, so it is on exactly when the page
 * is being developed:
 *
 * - `npm run dev` → always on. The Vite dev server proxies `/api` to :8080, so
 *   this is the same Portal backed by the same server.
 * - a built bundle → only with `VITE_AGENTATION=1 npm run build`. The Portal that
 *   ships is the one the Rust server serves out of `web/dist`; a feedback toolbar
 *   showing up there because someone forgot a flag is not a good trade.
 *
 * With the flag absent `import.meta.env.VITE_AGENTATION` is undefined, the whole
 * expression folds to `false`, and Rollup drops the component *and* the module
 * (the package declares `sideEffects: false`). The production bundle carries no
 * trace of it — `grep -r agentation web/dist` is the check.
 */
const showAgentation =
  import.meta.env.DEV || import.meta.env.VITE_AGENTATION === '1'

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
    {showAgentation && <Agentation appName="steno 控制中心" />}
  </StrictMode>,
)
