/// <reference types="vite/client" />

// Injected at build time by Vite (`define`) from package.json's version.
declare const __APP_VERSION__: string

// Self-hosted webfonts imported for their side effects (they inject @font-face
// rules). The packages ship CSS, not type declarations, so declare the specifiers.
declare module "@fontsource-variable/*"
declare module "@fontsource/*"
