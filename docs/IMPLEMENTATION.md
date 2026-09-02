# DSH Launcher — Implementation and Operations Notes

This document holds implementation and operations detail that is too dense for the
user-facing `README.md`. It is the reference for how the launcher behaves internally.

## Product scope (detailed)

- macOS arm64 and x64 DMG installers
- Windows x64 per-user NSIS installer; no portable ZIP distribution
- Pinned Node.js 24.19.0 archives admitted only by platform-specific SHA-256
- Exact `@deepseek-ai/dsh` installation; the first npm registry remains the version authority, availability is confirmed from the same complete version index used by npm, installation uses that release's verified tarball instead of a potentially stale package lookup, and later installs keep the successful source for cache reuse
- Transactional staging seeded from a valid installed Harness runtime when available and sufficient free space remains; the copied hidden lockfile is refreshed for npm reuse before executable smoke checks, atomic publication, startup recovery, and rollback, while seed-copy failures fall back to a clean candidate without changing the active runtime
- Live Harness installation phases for dependency resolution, package fetching, runtime writes, validation, and activation; prolonged npm silence explains that dependency calculation may still be active instead of treating missing log output as proof of a stall
- Stable terminal `dsh` command backed by the launcher's private runtime; the application appends its owned `bin` directory to the macOS login and interactive shell profiles or Windows user PATH after a successful Harness installation, without replacing an unrelated command entry
- Browser selection, system tray lifecycle, English/Simplified Chinese, and light/dark/system themes
- Plugin marketplace: consumes the [dsh-market](https://github.com/2BingLing/dsh-market) `plugins.json` through a daily validated snapshot at `market.dsdesktop.com` (fetched, hash-checked, validated, and cached in Rust; the CSP keeps the Webview from connecting directly) with Chinese search, type filters, sorting, and pagination. Cordis plugins install through the launcher's pinned Harness CLI (`plugin --profile web add/remove`; a pinned pnpm is provisioned into the isolated runtime when missing), skill plugins unpack from a validated GitHub tarball into `dsh-home/skills`, and uninstalls keep a recoverable backup. A three-state compatibility check against cordis peerDependencies runs before installation. Startup output exposes only strictly validated package names; a named installed profile dependency is transactionally removed and startup is retried once. The removal commits and produces a user-visible notice only when the retry succeeds; otherwise the complete profile batch is restored.
- Separate Harness updates and cryptographically signed desktop updates; Harness checks follow the persisted npm `latest` (Default) or `alpha` channel selected in Settings. Harness updates can run in the foreground with visible progress or prepare a validated candidate in the background while the current service keeps running. A prepared update is activated after confirmation, or automatically on the next launch if the app exits first. When service startup fails after activation, the retained validated previous runtime is exposed as a version-bound rollback action; rollback swaps the two runtimes without deleting either side and validates the restored runtime before updating the version marker.
- Startup repair is offered only when sanitized service output identifies a `session_projcache` schema failure or names an incompatible third-party package. With the managed service fully stopped, the launcher writes a repair manifest, moves each of the exact supported cache layouts (`storages/session_projcache.json` and `storages/session_projcache/`) to its private backup and restores it once as a recovery rehearsal, then isolates it for the retry. Session logs, `workspace.json`, settings, credentials, attachments, and every unrelated storage domain remain outside the mutation surface. Loader-named plugins join the marketplace rollback batch. A successful address publication commits the plugin batch and marks the old cache backup verified; a failed retry preserves newly generated cache evidence and restores the original cache and profiles.
- Finalized startup-repair backups are pruned only after a managed service has published a healthy address: retain seven days, at most the three newest verified repairs, and at most 512 MiB across finalized repair backups, deleting oldest first. The scanner accepts only direct `backups/startup-repair-*` directories with a readable finalized manifest and a regular file/directory tree; incomplete, malformed, special-file, and symlink-containing entries are reported as protected and never automatically removed. Settings reports eligible count, size, earliest expiry, and protected count. Manual cleanup uses the same validation and never targets migration backups or any path outside the backup root.
- Remote access: a sidebar page with master/LAN/public switches, QR codes, and rotatable 8-digit connection passwords. The self-contained authenticated reverse proxy in `dsh-core` (HTTP + WebSocket passthrough, per-IP and global login rate limiting, in-memory sessions) fronts the loopback-only Harness web UI; public access runs through a managed cloudflared quick tunnel admitted only by pinned SHA-256

## Remote access

The Remote page (sidebar → Remote) exposes the loopback-only Harness web UI to the operator's phone through the launcher's own authenticated reverse proxies; the Harness service itself stays bound to 127.0.0.1 and is never reconfigured. A master switch gates two independent scopes:

- **LAN**: after confirming that the machine has a usable non-loopback IPv4 route (Ethernet and Wi-Fi are both supported), a listener starts on all IPv4 interfaces; the QR code and address field show the primary LAN address, paired with an 8-digit connection password. Phones on the same LAN scan the code, enter the password once, and keep the session while the launcher stays running. Without a usable address, the LAN listener is not started and the UI disables its enable action. While the app is in the foreground, focus changes and a low-frequency refresh reconcile the route and listener, so connecting or disconnecting Ethernet/Wi-Fi updates availability without toggling remote access.
- **Public network**: a loopback-only listener fronted by a managed cloudflared quick tunnel (pinned release admitted by SHA-256, spawned shell-free with the launcher's proxy policy applied, no console window on Windows). Enabling requires an explicit security acknowledgement enforced by the backend; the random `*.trycloudflare.com` URL changes on every start, so an old link dies with its tunnel.

Both scopes share one proxy design: only the request head is parsed, the login issues an opaque HttpOnly session cookie held in memory (a launcher restart ends every session), repeated password failures lock the source address for 60 seconds with an additional global lockout against distributed attempts, and after authentication every remaining byte — form posts, streaming responses, and WebSocket frames — flows through a raw bidirectional pipe. Harness 0.1.2-alpha.2 and later print a private `/?token=...` launch URL that exchanges the per-process token for Harness's own browser cookie; the proxy retains this exact target in memory and performs the exchange once per remote session and Harness generation, only on the loopback hop after the launcher's password check. The token is never placed in a LAN/public URL, QR code, or remote redirect. Older bare-root URLs skip the exchange. Rotating a password revokes that scope's sessions immediately and drops its live connections. Because the complete upstream endpoint is resolved per connection, Harness restarts and updates re-bootstrap existing launcher sessions when needed without replacing the remote listener or open tunnel. Remote passwords live in the desktop-owned `remote/` directory, never in DSH_HOME.

Login brute-force protection is in-memory: five failed attempts from one address lock it for 60 seconds (a successful login clears the record), and 30 failures across many addresses within a 60-second window trigger a short global lockout.

## Proxy support

Settings → Proxy controls how the launcher itself reaches the network — Harness registry queries, tarball and Node.js downloads, release source checks, the marketplace catalog/registry/GitHub clients, networked npm/pnpm/Harness subprocesses, and desktop update checks and downloads all share one configuration (the Tauri updater is adapted through its `configure_client` hook, so checks and downloads use the same proxy plan). Three mutually exclusive modes are available:

- **System** (default, including for configurations written before proxy support): uses proxy environment variables (`HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` and their lowercase forms, the first valid value across casings — a scheme-less traditional `host:port` is treated as an HTTP proxy) plus the operating system proxy where reqwest exposes one. On macOS the HTTP clients and updater merge environment variables with the OS system proxy; on Linux reqwest's system mode is environment-driven. Subprocesses receive the environment-derived proxy variables only (an OS system proxy has no environment-variable form and is never exported). On Windows, where the system-proxy matcher cannot handle per-protocol registry entries, the launcher resolves one merged per-protocol plan for the clients, the updater, and subprocesses: per protocol the matching environment variable wins, then `ALL_PROXY`/`all_proxy`; only protocols the environment did not cover are filled from the current user's Internet Settings (`ProxyEnable`, `ProxyServer`, `ProxyOverride`, read strictly read-only), converting single-address or `http=...;https=...;socks=...` entries into npm/pnpm-compatible variables and expanding `<local>` to explicit loopback bypass entries (an approximation for npm/pnpm, not a replica of WinInet's local-name semantics). `NO_PROXY`/`no_proxy` wins over `ProxyOverride`. In a CGI environment (`REQUEST_METHOD` set) no proxy source is trusted at all. The launcher never modifies the registry or the system proxy.
- **Direct**: always connects directly, ignoring the system proxy and every proxy environment variable; subprocesses receive a proxy-free environment.
- **Manual**: one proxy URL (`http`, `https`, `socks5`, or `socks5h`; `scheme://host[:port]` only) applied to all launcher traffic, the updater (checks and downloads), and subprocesses, with an optional bypass (NO_PROXY) list accepting domains, IPs, and IPv4/IPv6 CIDR ranges. IE-style `*.domain` entries are normalized to the equivalent leading-dot domain rule that reqwest, curl, and npm understand; other wildcard forms are dropped rather than passed to reqwest.

Saved settings apply immediately to launcher requests, Retry, and newly spawned subprocesses. Because an existing process environment cannot be changed in place, the settings page offers an explicit Harness restart when the service is already running; it never interrupts that session silently. The **Test connection** action checks the values currently in the form — saved or not — against both Harness registries and reports each source with a classified, sanitized network error (timeout, proxy authentication required — including CONNECT tunnel 407s — TLS/certificate, connection/DNS, or HTTP status). Proxy usernames and passwords in URLs are rejected and never stored, inactive manual fields are erased in System/Direct mode, and diagnostics never echo URL userinfo; PAC/WPAD auto-configuration and NTLM/Kerberos integrated proxy authentication are not supported in this version.

## Desktop pet

The desktop pet is a first-class Launcher feature rather than an installable Harness plugin. Its delivery is split into six completed layers:

- **M0 — state contract:** one reducer maps top-level Harness session events onto exactly five public states (`waiting`, `error`, `working`, `thinking`, `idle`). User questions and approvals become waiting, tool execution becomes working, model output becomes thinking, failures become error, and completed/aborted turns become idle. Across concurrent sessions the priority is waiting → error → working → thinking → idle; subagents never replace the top-level display.
- **M1 — Harness bridge:** the self-contained `pet-bridge.mjs` module is staged next to the existing balance bridge and injected with the generated `dsh web --patch` overlay. It publishes only bounded, sanitized activity metadata through a random-token-protected loopback SSE endpoint. The token remains in the child environment and is never placed in a URL or frontend payload.
- **M2 — desktop service:** `dsh-core::pet` strictly parses versioned snapshots, rejects unknown fields and invalid lengths/progress, reconnects with bounded backoff, and exposes connected/stale/unavailable bridge health through typed Tauri commands and `pet://state`. A combined-overlay startup failure retries once with the balance-only overlay before falling back to unpatched Harness, so the optional pet can never prevent the workspace from starting.
- **M3 — pet window:** Tauri creates a separate transparent, undecorated, always-on-top window. The React renderer lazy-loads Lottie, switches animation data for the five states, supports reduced motion and localized bubbles, lets the user drag the whole window, saves physical screen coordinates, clamps restored coordinates to the available monitor bounds, and hides whenever Harness is not ready. Click-through applies to the whole window and is always reversible from the main Desktop Pet page.
- **M4 — product controls:** the feature registry owns the sidebar page and route. The page selects a catalog pet, previews every state without changing live state, toggles visibility, bubble, scale, reduced motion, and click-through, and reports bridge health. Preferences are atomically persisted in `preferences.json`; pre-feature files deserialize with the pet disabled. The tray menu mirrors the show/hide control.
- **M5 — catalog and verification:** built-in assets live under repository-root `pets/`. `pets/config.json` declares `count` and bilingual nickname, species name, tags, description, optional per-state bubble copy, resource folder, and all five animation files. Missing bubble copy falls back to the bilingual Launcher dictionary. Catalog, reducer, stream-reset, strict payload, preference, IPC-generation, lint, build, and Rust tests cover the implementation.

The initial catalog contains Gru's supplied marmot. Runtime packaging includes only its five Lottie JSON files and referenced PNG layers. Generator/QA artifacts are excluded. These visual assets are excluded from the repository MIT License and may be used and redistributed only for non-commercial purposes under `pets/ASSET-LICENSE.md`; commercial use requires Gru's prior written permission.

## Architecture

```text
React feature registry + HashRouter
  └─ typed launcher API / revisioned state event
      └─ Tauri command and lifecycle adapter
          └─ dsh-core application services
              ├─ runtime deployment and rollback
              ├─ source/CC Switch import
              ├─ managed dsh web process tree
              ├─ plugin marketplace (catalog cache, query, install/uninstall, installed detection, compatibility checks)
              ├─ remote access proxies and cloudflared tunnel
              ├─ desktop pet reducer, loopback SSE client, and preferences
              └─ browser and preferences ports
                  └─ pinned Node.js → published @deepseek-ai/dsh
```

The feature registry owns routes and navigation metadata. Adding future pages means adding a feature descriptor and its backend module, not widening one global view. Business rules live in `dsh-core`, which has no Tauri dependency. Tauri owns only OS lifecycle, tray, clipboard, updater integration, and typed IPC. Commands and events are module-namespaced, and the frontend accepts only monotonically newer snapshot revisions. Windows builds use the GUI subsystem and start every helper process without a console window. When the system tray is available, closing the main window hides it and normal application exit is available from the tray menu. A tray initialization failure does not block startup; a missing bundle icon is handled through the same fallible path instead of panicking. In that fallback mode, closing the main window performs a full cleanup and exits. If the initial background startup worker cannot be created, the desktop shell remains open in a retryable failed state; a rejected eager IPC initialization is consumed while the React error boundary presents the fatal UI, so neither path becomes an unhandled startup failure. Full exit is not released until deployment and local-service process trees have stopped and the loopback service port has closed. A per-home instance lock prevents concurrent launchers and waits briefly for an updating instance to release ownership. Before starting on macOS or Windows, the launcher verifies executable paths, command arguments, and user ownership, then reclaims every stale service created from its private runtime; macOS additionally verifies process-group ownership. Services run behind a parent-pipe guard that exits after terminating the service when the launcher is killed. Unix guards terminate the isolated process group, while Windows guards combine nested kill-on-close Job Objects with a direct parent-pipe fallback.

The project intentionally avoids a runtime plugin system for the launcher itself, a generalized workflow engine, or duplicate frontend/backend state machines. Those abstractions are not needed for the current launcher and would make future changes harder rather than easier. The plugin marketplace manages the Harness's own plugin ecosystem through the pinned Harness CLI and the npm/pnpm layer; it never enters the launcher's process tree or state machine.

## Data compatibility and safety

The Rust application preserves the existing disk protocol:

```text
~/.dsh-desktop/
├── runtime/{node,dsh,runtime.version,.deployment.lock}
├── cache/
├── dsh-home/
├── bin/dsh[.cmd]
├── server.log
├── install.log
├── server.pid
├── language
├── preferences.json
├── backups/migration-*/dsh-home
├── .migration-complete-v1
└── .migration-skip-v1
```

After the first successful Harness installation, open a new terminal before running `dsh --version`. macOS login and interactive shell configuration is kept in clearly marked managed blocks, and Windows receives an environment-change notification. An explicit `DSH_DESKTOP_HOME` still creates the command wrapper inside that isolated home but deliberately skips automatic user PATH/profile changes, keeping development and test homes isolated.

An explicit `DSH_HOME` disables all imports. Otherwise, the launcher only discovers compatible data in `DSH_DESKTOP_SOURCE_HOME` (default `~/.dsh`) and presents a choice before copying anything. Approval creates and verifies a private backup, performs a restore rehearsal, builds the complete result away from the active home, and publishes it with a crash-recoverable atomic transaction. Skipping is persisted and starts without importing into the existing isolated launcher home. Existing destination values and populated workspace ledgers always win.

CC Switch remains an optional read-only source. The importer opens `cc-switch.db` read-only, accepts only standalone Claude providers with a literal credential, non-loopback HTTP(S) endpoint, supported protocol, and at least one model, and skips OAuth, managed, proxy-dependent, and ambiguous rows. Existing documents are conservatively extended only when their structure is understood. Credentials go only to `.credentials.yaml`, never settings or logs. Two-file publication is rolled back to the exact original bytes on failure. If Windows permissions or another local I/O problem prevents this optional import, the launcher reports that it was skipped and continues installing and starting Harness.

Tests, checks, builds, and packaging must set temporary `DSH_DESKTOP_HOME`, `DSH_HOME`, `DSH_DESKTOP_SOURCE_HOME`, and `DSH_DESKTOP_CC_SWITCH_HOME`. They must never touch real user homes, Keychain, credential stores, or production data.

Harness updates continue to reuse the private npm download cache, but `cache/npm` is checked before and after installations and removed as soon as it reaches 1 GiB. Old pinned Node archives and interrupted archive downloads are also pruned, while the current verified Node archive remains reusable. `install.log` and `server.log` are each bounded to 16 MiB. These policies never touch `dsh-home`, settings, sessions, or credentials. Runtime storage contains the active version, one previous rollback version, and—only while a background update is ready—the isolated validated candidate.

The plugin marketplace consumes the dsh-market catalog read-only and caches it under `cache/marketplace`, refreshing by its `generatedAt` daily. At 07:00 Asia/Shanghai a public-repository workflow resolves an immutable GitHub commit, verifies that its history descends from the embedded trust anchor, validates the catalog, and publishes it through a bounded two-slot R2 snapshot. Clients fetch only `market.dsdesktop.com`, verify the manifest size and SHA-256, and retain the last verified cache when refresh fails; malformed entries are quarantined individually. Every install and uninstall stays inside the isolated `dsh-home`: Cordis changes are prepared and validated in a complete candidate profile before directory-level publication, while skill uninstalls move the selected directory into `cache/marketplace/trash` instead of deleting it. Before installation, the launcher resolves and pins the exact npm version or Skill commit, verifies the npm `repository` and catalog source fields against the disclosed GitHub ID, and displays the source, target, version, binding status, and execution risk. Uninstall removes only the explicitly selected profile or skill copy.

## Development and website

Prerequisites: Node.js 24+, pnpm 10.12.3, and Rust 1.96.

`pnpm bindings` generates [bindings.ts](../src/platform/generated/bindings.ts) from the Rust domain types. Commit the generated result and verify locally that regenerating it produces no diff. `pnpm deadcode` enforces the frontend dependency boundary; strict Clippy does the same for Rust.

The repository-level `public/` directory is the standalone product website. Vite has `publicDir: false`, so the website and desktop assets cannot be accidentally mixed. Website code remains plain HTML/CSS/JavaScript. Cloudflare Workers Builds must use `public` as the root directory, no build command, and `npx wrangler deploy` as the deploy command. The Worker proxies the published GitHub updater manifest at `/latest.json`; installed clients try this endpoint first and fall back to GitHub directly. Update packages and their mandatory signatures remain GitHub Release assets. The separate Standard-class R2 bucket `dsh-launcher-marketplace` is exposed read-only at `market.dsdesktop.com`, with `r2.dev` disabled and JSON cache rules enabled. Marketplace publication uses the S3-compatible API with bucket-scoped `R2_ACCESS_KEY_ID` and `R2_SECRET_ACCESS_KEY` secrets plus `CLOUDFLARE_ACCOUNT_ID`; fixed `latest.json`, `catalog-a.json`, and `catalog-b.json` objects bound storage growth within the expected free allowance. `pnpm cloudflare:check` guards these contracts and all local website assets without changing the existing automatic deployment path.

To provision the marketplace channel, create the bucket with the Standard storage class, connect only the `market.dsdesktop.com` custom domain, disable its `r2.dev` URL, and add a Cache Everything rule for `/v1/*.json` that keeps the full query string in the cache key. Set a low R2 budget alert, create an Object Read & Write R2 token limited to this bucket, add its access key, secret key, and the account ID as Actions secrets, then manually run **Publish Plugin Marketplace** once with `bootstrap=true`. Later scheduled runs read `latest.json` directly through the authenticated S3-compatible R2 API, never through a possibly stale CDN response.

## Release and signing

All versions in `package.json`, workspace `Cargo.toml`, and `src-tauri/tauri.conf.json` must match the `desktop-v<version>` tag. `pnpm versions` enforces this rule.

Tauri updater signatures are mandatory even when platform signing is unavailable:

1. Create the Git-ignored local key directory and generate the updater key pair with an explicit **file** path (the `-w` target is not a directory):

   ```sh
   mkdir -p signer-keys
   chmod 700 signer-keys
   pnpm tauri signer generate -w signer-keys/dsh-launcher-updater.key
   ```

   Never use `--force` on an existing updater key without a separately reviewed rotation and recovery plan. Never commit the private key, and keep a verified encrypted backup outside the repository.

2. Store the private key and optional password in GitHub secrets `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`.
3. Store the complete contents of `signer-keys/dsh-launcher-updater.key.pub` in the GitHub Actions variable `TAURI_UPDATER_PUBLIC_KEY`.
4. Before tagging, run the complete common gate locally with temporary `DSH_DESKTOP_HOME`, `DSH_HOME`, `DSH_DESKTOP_SOURCE_HOME`, and `DSH_DESKTOP_CC_SWITCH_HOME` paths: `pnpm versions`, regenerate and diff-check bindings, `pnpm format:check`, `pnpm lint`, `pnpm test`, `pnpm deadcode`, `pnpm cloudflare:check`, `pnpm build`, `cargo fmt --all -- --check`, `cargo test --workspace --all-targets`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo run --quiet -p dsh-core --example release_check`.
5. Push `desktop-v<version>`. CI runs only the native platform regressions needed by the release matrix—Windows tests in the Windows package job and macOS-only tests in the arm64 Mac job—then builds and signs isolated macOS arm64/x64 and Windows x64 artifacts without publishing from matrix jobs. One final job stages versioned, architecture-specific names, creates a clean draft GitHub Release, uploads all assets serially, verifies every installer, updater archive, signature, manifest entry, and exact website download URL, and only then publishes the verified release. A failed or incomplete build remains unpublished, so `releases/latest/download/latest.json` and installed clients never observe a partial release.

The checked-in updater public key is intentionally empty: local source builds do not belong to a production update channel. Release CI validates the configured minisign public key, writes a temporary release-only Tauri config, and passes it explicitly to the CLI with `--config`; a release cannot build without both sides of the updater trust chain. In the absence of a Developer ID, macOS bundles receive a complete ad-hoc signature and both local packaging and CI reject bundles that fail strict `codesign` verification. Ad-hoc signing does not provide Apple notarization: a browser-downloaded build may still require the user to approve its first launch in macOS Privacy & Security. A warning-free first launch on arbitrary Macs requires a Developer ID Application certificate and notarization. Windows Authenticode remains optional independent hardening and does not weaken the mandatory Tauri update signature.
