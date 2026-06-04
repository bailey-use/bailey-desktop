# aivo plugin protocol

**Protocol version `1`.** A plugin adds a top-level command to aivo: a standalone executable
`aivo-<name>`, in **any language**, that aivo runs as `aivo <name>`.

```bash
aivo plugins install ./aivo-hello   # add it (local file or http(s):// URL)
aivo hello --flag                   # aivo runs `aivo-hello --flag`
aivo plugins list                   # what's installed, with version / roles / caps
```

The only thing required to be a plugin is to exist and run. Optionally it can **self-describe**
(below) to report its version, roles, and declared capabilities.

## Discovery

`aivo <name>` runs `aivo-<name>` when `name` isn't a built-in, a known tool, a chat ref, or an
alias — built-ins always win. Lookup order: `~/.config/aivo/plugins/` (managed by
`aivo plugins install`) → the directory of the `aivo` binary → `$PATH`. On Windows the file is
`aivo-<name>.exe`.

When aivo runs a plugin it sets `AIVO_CONFIG_DIR` (aivo's config dir; the key store is
`<dir>/config.json`) and, under `--debug`, `AIVO_DEBUG_LOG`.

## The manifest (self-description)

When run as `aivo-<name> --aivo-manifest`, a conforming plugin prints **one JSON object** to
stdout, exits `0`, and does nothing else (no network, no child processes). aivo probes this at
install time (~2s timeout) and caches the result. A plugin that doesn't implement it still
installs and runs — it just has no recorded metadata. Self-description is opt-in.

> aivo probes **local-path installs only** — it won't execute a freshly-downloaded `http(s)://`
> binary to read its manifest (that waits on the consent gate noted under *Reserved*). URL
> installs are recorded without a manifest until reinstalled from a local path.

```jsonc
{
  "name": "amp",            // required; must match the installed name
  "version": "0.1.0",       // required
  "protocol": "1",          // required; the protocol this targets
  "description": "…",       // optional
  "roles": ["subcommand"],  // optional; "subcommand" today ("hook" reserved)
  "capabilities": [],       // optional; declared only — see below
  "homepage": "…"           // optional
}
```

Unknown fields are ignored (forward-compatible). A `protocol` the host doesn't speak is treated
as "no manifest" (the plugin still runs; its declared metadata is ignored). The installed name
always wins over the manifest's `name`.

A complete plugin, in shell:

```sh
#!/bin/sh
if [ "$1" = "--aivo-manifest" ]; then
  printf '%s\n' '{"name":"hello","version":"1.0.0","protocol":"1","roles":["subcommand"]}'
  exit 0
fi
echo "hello: $*"
```

## Capabilities (declared, not yet enforced)

A manifest may declare what host power it wants. **In v1 this is disclosure only** — aivo shows
the list at install and records it; nothing is granted or enforced. Vocabulary: `endpoint` (a
scoped, routed aivo endpoint, without the raw key), `raw-key` (the resolved key), `config-read`,
`config-write`, `spawn`, `hook:<event>`. Default: none.

## Registry

Per-plugin provenance lives in `~/.config/aivo/plugins/.registry.json` (a dotfile):

```jsonc
{
  "version": 1,
  "plugins": {
    "amp": {
      "source":       "/abs/path-or-url",        // for `update`
      "checksum":     "sha256:9f86d0…",          // of the installed bytes
      "installed_at": "2026-06-04T05:48:30Z",    // RFC3339
      "manifest":     { /* … */ }                // absent if not self-described
    }
  }
}
```

An older source-only `.sources.json` is migrated automatically. The `sha256` is recorded now;
verifying it on every run, and signing, are future work.

## Security

A plugin runs with **your full privileges** — aivo can't sandbox it. Treat `aivo plugins install`
like `npm i -g`: install only what you trust. aivo's protections are **consent + provenance**
(disclosure at install, the `sha256` pin), not containment.

## Reserved for later phases

Specified so the contract stays stable, but not yet implemented: a **`hook` role** (observe and
transform launches/routing over JSON-RPC), a **scoped control endpoint** (`AIVO_ENDPOINT_URL` +
an ephemeral token → routed access without the raw key) that makes capabilities *enforceable*,
and **signing / a discovery index**.

## Commands

```bash
aivo plugins list
aivo plugins install <path|url> [--name N] [--force]
aivo plugins update [name]      # re-fetch from the recorded source
aivo plugins remove <name>
```
