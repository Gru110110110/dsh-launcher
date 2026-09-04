# Desktop pet catalog

`config.json` is the single catalog for built-in desktop pets. `count` must
match the number of entries in `pets`, every pet id/folder must use lowercase
ASCII letters, digits, `_` or `-`, and every pet folder must provide one
Lottie JSON animation for each public state: `waiting`, `error`, `working`,
`thinking`, and `idle`.

Localized catalog strings use `{ "zh": "…", "en": "…" }`. Bubble text may
be omitted; the Launcher supplies localized defaults. Image paths referenced
by Lottie JSON files must remain below that pet's folder.

The `marmot` (Mochi / 麻薯) and `orange-cat` (Juzi / 橘子) runtime assets were
created and supplied by **Gru**. Only
the five approved runtime animations and their image layers are included here;
generator scripts, validation outputs, preview tooling, and platform metadata
are intentionally excluded.

`orange-cat` preserves the supplied v4 five-state package, including the known
tail seam in its waiting animation. Its bubble text uses the localized defaults.

The desktop pet visual assets are **not covered by the repository's MIT
License**. They may be used and redistributed only for non-commercial purposes
under [ASSET-LICENSE.md](ASSET-LICENSE.md). Commercial use requires Gru's prior
written permission.
