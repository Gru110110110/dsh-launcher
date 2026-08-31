# AGENTS.md

DSH Launcher is a React/Tauri/Rust launcher for DeepSeek Harness that installs and runs the published `@deepseek-ai/dsh` package. Keep it independent from the Harness source workspace: integration happens through the published CLI and its documented output.

## Data safety

- Tests, builds, packaging, and install checks must use temporary `DSH_DESKTOP_HOME`, `DSH_HOME`, and source-home paths. They must never read, write, copy, migrate, delete, or overwrite real `~/.dsh-desktop`, `~/.dsh`, Keychain, credential stores, or production user data.
- Treat activation codes, encryption keys, tokens, configuration, sessions, attachments, and workspace ledgers as user data. Never mutate or migrate real instances without read-only discovery, impact analysis, a verified backup and restore rehearsal, and explicit user approval for the exact target and action.
- Preserve an active runtime and user data when deployment or import fails. Candidate runtime content is staged and validated before publication; import copies only supported entries and never replaces an existing destination.
- Keep test fixtures physically isolated from production paths. Local loopback servers and fake runtimes belong in temporary directories and must be stopped by the owning test.

## Commands

```sh
pnpm bindings
pnpm lint
pnpm test
pnpm deadcode
pnpm build
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
```

Run every Rust test, Tauri build, and package check with isolated temporary desktop, Harness, source, and CC Switch homes. `pnpm tauri build` is the packaging entry point; release CI produces macOS DMGs and a Windows per-user NSIS installer.

Run the macOS `process_recovery` / startup-recovery stress checks locally before tagging. Do not run them in `.github/workflows/desktop.yml`; hosted macOS release runners are reserved for target compilation, signing, packaging, installed-package smoke checks, and publication.

## Conventions

- Keep business behavior in `dsh-core` without a Tauri dependency. Tauri commands adapt OS capabilities; React features own their route and navigation metadata.
- Generate the frontend IPC contract from Rust with `pnpm bindings`; do not maintain duplicate handwritten domain types.
- Keep subprocess calls shell-free and pass URLs and paths as separate arguments.
- Validate downloaded Node archives against the pinned SHA-256 before extraction, reject archive traversal and links outside the expected top-level directory, and install an exact Harness version.
- Keep English and Simplified Chinese UI dictionaries structurally identical. Update `README.md` and `README.zh.md` together.
- Keep `README.md` / `README.zh.md` user-facing and concise. Detailed internal implementation, operations, release/signing, and deployment content must go into `docs/IMPLEMENTATION.md` / `docs/IMPLEMENTATION.zh.md`, never into the README. Update the README and the matching `IMPLEMENTATION` doc together in both languages.
- Tauri updater signatures are mandatory for releases; platform code signing remains optional. Never commit the updater private key.
- Do not commit `node_modules/`, `target/`, `dist/`, TypeScript build info, or platform metadata.
