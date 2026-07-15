# Bailey Desktop release boundary

Bailey Desktop and upstream Aivo are released independently. Desktop is
versioned by `desktop/package.json`, Tauri config, and the Desktop Cargo crate.
The root `Cargo.toml` stays on upstream Aivo `0.39.0` and must not be bumped for
a Desktop release.

Developer prereleases use `.github/workflows/desktop-release.yml` and
`desktop-build-*` tags. Production uses
`.github/workflows/desktop-production-release.yml` and a
`bailey-desktop-v<desktop-version>` tag. Production fails closed unless Apple
signing/notarization and Windows Authenticode material are configured; both
channels publish one macOS installer, one Windows installer, and matching
portable SHA-256 sidecars whose entries contain only the installer filename.
The upstream `.github/workflows/release.yml` remains guarded
to `yuanchuan/aivo`, so Bailey tags cannot publish upstream npm/R2/Homebrew
channels.

The Local Tools dependency is reproducible: each Desktop workflow checks out
`bailey-use/bailey-use` at the full commit SHA in that workflow, builds the
platform CUA archive, and sets `BAILEY_USE_SOURCE_DIR`. Update that SHA only
after the intended Local Tools commit is pushed. A release must never depend on
a developer-machine sibling checkout.

Production secrets:

- Apple: `APPLE_CERTIFICATE_BASE64`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`, `APPLE_ID`, `APPLE_PASSWORD`, `APPLE_TEAM_ID`.
- Windows: `WINDOWS_CERTIFICATE_BASE64`, `WINDOWS_CERTIFICATE_PASSWORD`.
- Cross-repository source: `BAILEY_USE_DEPLOY_KEY` contains a dedicated SSH
  deploy key whose public half is registered read-only on
  `bailey-use/bailey-use`. The private key is not persisted by checkout and
  must not be reused for another repository or purpose. Desktop packaging in
  pull-request CI is limited to non-Dependabot branches in this repository;
  fork pull requests never receive this credential.

The integrated installer copies its signed immutable runtime resource to a
versioned user-data directory, registers the Native Host without opening a
browser, and launches Local Tools using a bundled Node executable plus a
separate argv array. Its manifest records Desktop/Local Tools versions, MCP
protocol, extension, CUA driver, and compatibility status. The packaged runtime
also carries the upstream Cua Driver MIT license under `licenses/cua-MIT.txt`;
the release build fails if that notice is missing. The manifest also records
the size and SHA-256 digest of every runtime file. Desktop validates both the
signed resource and its user-data copy before execution, atomically replacing
a stale or modified copy.

Product MCP `_meta` is retained outside the model-visible tool schema.
`bailey/effect`, `bailey/approval`, and `bailey/targetFields` drive policy.
External sends always require a fresh approval bound to SHA-256 of the exact
tool name and arguments; only allow/deny is offered. Cloud Record sync is an
asynchronous side channel and excludes cwd, prompts, arguments, assistant text,
DOM, screenshots, local paths, URLs, and evidence content.

Model access and record synchronization use separate credentials. Production
Desktop reads both from its OS credential entry: the model key provisions the
Provider, while the records-only key stays in the native relay. Desktop never
reuses the model credential for `/api` writes. The legacy
`BAILEY_CLOUD_RECORDS_API_KEY`, `BAILEY_CLOUD_RECORD_BASE_URL`, and
`BAILEY_DISABLE_CLOUD_RECORDS` environment contract remains available to
standalone Aivo App Server launches, but packaged Desktop deliberately does not
pass a records key into Aivo.

Production Desktop obtains both credentials from Bailey Cloud through the
Tauri shell's in-app account flow, stores them in Bailey's operating-system
credential entry, and gives the model key only to a short-lived, task-less
invocation of Aivo's existing Provider bootstrap. That process updates Aivo's
encrypted Provider store and exits before the real Agent Runtime starts. The
records key stays in Tauri's native record relay. Neither key is present in the
long-running Aivo environment, so shells, jobs, hooks, and MCP children cannot
inherit it. Do not add Bailey login endpoints or token lifecycle code to Aivo
App Server or AgentEngine, and do not open an external browser for the normal
login flow.
