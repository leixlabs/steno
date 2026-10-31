/// <reference types="vite/client" />

// `import.meta.env` needs the Vite client types; without this reference `tsc -b`
// rejects it under `strict`. Declaring the one build-time flag the app reads also
// documents it where an editor will show it.
interface ImportMetaEnv {
  /** `1` builds the Portal with the Agentation annotation toolbar included. */
  readonly VITE_AGENTATION?: string
}

interface ImportMeta {
  readonly env: ImportMetaEnv
}
