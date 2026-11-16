Stack: Next.js 15 (App Router), React 19, TypeScript in strict mode. No other packages are installed, so do not import any.

Each page owns the directory `app/<page id>/` and nothing else. Its route file is `app/<page id>/page.tsx`, a default-exported React component, so page `orders` is served at `/orders`. A page that needs client state starts its file with `"use client";`.

`app/layout.tsx` and `app/globals.css` are shared and already exist; no page edits them. There is no `app/page.tsx`: if the product needs something at `/`, plan a contract file `app/page.tsx` that links to the pages.

Shared types, data and components go in contract files under `lib/` or `components/`, imported as `@/lib/...` and `@/components/...`. Pages never import from each other.
