# Aivo App Server Protocol v1

`aivo app-server --stdio` exposes the in-process `AgentEngine` to desktop and
IDE clients. It is a long-lived process; the GUI is a client, not the owner of
the agent loop.

## Transport

- UTF-8 JSON-RPC 2.0 over stdin/stdout.
- Exactly one JSON object per line.
- stdout is protocol-only. Diagnostics go to stderr.
- The maximum inbound frame is 1 MiB.
- API secrets, provider base URLs, and protocol-routing details are resolved
  inside Aivo and never cross the protocol. The provider catalog exposes only
  stable local ids, display names, availability, and selected-model metadata.

The client starts with:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"bailey-desktop","version":"0.1.0"}}}
```

The server accepts exactly protocol version `1` and reports its current
capabilities. `health/check` is the only method allowed before initialization.

The `models` capability reports support for provider discovery, model listing,
and provider/model selection when a thread is created.

The `mcp` capability reports real external-tool support. Protocol v1 loads only
enabled servers from Aivo's user-level `~/.config/aivo/mcp.json`, at thread
initialization. Both stdio and Streamable HTTP transports are supported;
previously stored OAuth credentials may be used, but App Server does not start
an interactive OAuth flow. Pack and project `.mcp.json` files are not read, so
opening a repository cannot implicitly start a project-provided command. MCP
connection and configuration failures are best-effort and do not prevent the
thread from opening. If Aivo cannot read the user's MCP enable/disable
preferences, loading fails closed with zero MCP tools and a degraded summary.

```json
{"mcp":{"tools":true,"configScopes":["user"],"projectConfiguration":false,"transports":["stdio","streamableHttp"],"oauth":{"storedCredentials":true,"interactive":false},"load":"thread","bestEffort":true}}
```

## Client methods

### `health/check`

Returns `starting`, `ready`, or `draining`, the process id, protocol version,
and in-memory thread count.

### `provider/list`

```json
{"jsonrpc":"2.0","id":2,"method":"provider/list","params":{}}
```

Returns the model providers already configured in Aivo. It never returns a
secret or base URL:

```json
{"activeModelProvider":"provider_...","data":[{"id":"provider_...","displayName":"My Gateway","kind":"openai_compatible","active":true,"agentCompatible":true,"selectedModel":"gpt-5"}]}
```

`agentCompatible: false` means the credential belongs to an external CLI or
ACP runtime and cannot drive Aivo's in-process `AgentEngine`.

### `model/list`

```json
{"jsonrpc":"2.0","id":3,"method":"model/list","params":{"modelProvider":"provider_...","refresh":false}}
```

The server resolves only the selected provider's credential. The result is
cache-first and contains `data`, `selectedModel`, and the public provider name.
Providers without a model-catalog endpoint return the persisted selected model
plus a fixed `warning`; clients may still accept a manually entered model id.
`catalogAvailable` distinguishes that fallback from a real catalog, and
`selectedModelAvailable` reports whether a persisted default still appears in a
successfully fetched catalog. Raw provider errors and URLs are never returned.
Catalog fetches run outside the stdin request loop, so cancellation, approval
responses, and shutdown remain responsive; JSON-RPC responses may arrive out of
request order.

### `thread/start`

```json
{"jsonrpc":"2.0","id":4,"method":"thread/start","params":{"cwd":"/work/project","modelProvider":"optional-local-provider-id","model":"optional-model-id"}}
```

`modelProvider` and `model` fall back to Aivo's active provider and its selected
coding model. `keyId` remains an input alias for older clients. Provider
selection is fixed for the lifetime of this v1 thread; a different provider
starts a new thread.
Starting a thread creates its durable session before returning. The response
contains `threadId`, `sessionId`, canonical `cwd`, `model`, `title`, and a
sanitized `provider` object such as
`{"kind":"openai_compatible","label":"OpenAI-compatible"}`. Credential ids,
names, secrets, and base URLs are not returned with the thread. A runtime thread
also reports a sanitized MCP summary such as
`{"scope":"user","connectedServers":1,"tools":6,"issues":0,"degraded":false}`.
It exposes only aggregate counts; server names, commands, URLs, credentials, and
raw connection errors remain local. A runtime thread allows one active turn.
The durable session survives `thread/close`, server shutdown, and desktop
restart. Its title is the first non-empty line of the first accepted user
message, truncated to 34 Unicode characters with an ellipsis when needed.

### `thread/list`

```json
{"jsonrpc":"2.0","id":5,"method":"thread/list","params":{"cwd":"/work/project"}}
```

The cwd is canonicalized and the response contains every durable session in
that project, regardless of which local key created it. Rows are newest first:

```json
{"data":[{"sessionId":"session_...","cwd":"/work/project","provider":{"kind":"aivo_starter","label":"Aivo Starter"},"model":"aivo/starter","title":"Fix the failing test","preview":"...","updatedAt":"...","createdAt":"..."}]}
```

Key ids, key names, and base URLs are never returned.

### `thread/resume`

```json
{"jsonrpc":"2.0","id":6,"method":"thread/resume","params":{"sessionId":"session_..."}}
```

Resume creates a new in-memory runtime backed by the existing durable session.
It restores the session's stored provider and model, reloads the current
user-level MCP configuration, and returns `threadId`, `sessionId`, canonical
`cwd`, sanitized `provider`, the aggregate `mcp` summary, `model`, `title`, and
display messages. Exact AgentEngine messages (including tool call/result ids)
are restored when available; older sessions fall back to their user/assistant
text history. A kernel-backed lease allows only one app-server process to load
a session at a time. Process exit and crashes release the lease automatically;
stale lock-file paths are safe to reuse.

### `thread/delete`

```json
{"jsonrpc":"2.0","id":7,"method":"thread/delete","params":{"sessionId":"session_..."}}
```

Deletes an unloaded durable session and its artifacts. A loaded session returns
`THREAD_BUSY`; callers must successfully `thread/close` its runtime first. The
operation is idempotent and returns `state: "deleted"` or `state: "not_found"`.
It never returns key or provider metadata.

### `thread/flush`

```json
{"jsonrpc":"2.0","id":8,"method":"thread/flush","params":{"threadId":"thread_..."}}
```

Retries durable persistence from the currently loaded in-memory conversation
and AgentEngine transcript without running another agent turn. It returns
`persisted: true` on success and `THREAD_BUSY` while a turn is active.

### `thread/close`

```json
{"jsonrpc":"2.0","id":9,"method":"thread/close","params":{"threadId":"thread_..."}}
```

Stops any active turn, releases background jobs and removes only the in-memory
runtime. It does not delete the durable session. Desktop clients close the
previous runtime after a replacement or resume has been created successfully.

### `turn/start`

```json
{"jsonrpc":"2.0","id":10,"method":"turn/start","params":{"threadId":"thread_...","text":"Fix the failing test"}}
```

The server acknowledges immediately with a `turnId`; the agent continues on a
background task. Events arrive as notifications:

```json
{"jsonrpc":"2.0","method":"event","params":{"schemaVersion":1,"seq":1,"threadId":"thread_...","turnId":"turn_...","type":"turn.started","createdAt":"2026-07-10T06:00:00.000Z","payload":{}}}
```

Event types in v1:

- `turn.started`
- `assistant.text.delta`
- `assistant.reasoning.delta`
- `context.updated`
- `plan.updated`
- `tool.started`
- `tool.completed`
- `notice`
- `error`
- `usage.updated`
- terminal: `turn.completed`, `turn.failed`, `turn.stopped`, `turn.cancelled`

`seq` is monotonic per thread. Every accepted turn emits exactly one terminal
event.

### `turn/cancel`

Cancellation is idempotent. It stops model/tool work, denies pending
interactions, and persists the interrupted AgentEngine transcript before the
terminal event so completed tool results are not replayed after resume. It does
not claim to roll back side effects that already happened.

### `shutdown`

The response is flushed before active turns are cancelled and the process
exits successfully. Stdin EOF has the same fail-closed cleanup behavior.

## Server requests

Mutating or sensitive tools use a real reverse JSON-RPC request:

```json
{"jsonrpc":"2.0","id":"server:1","method":"approval/request","params":{"schemaVersion":1,"threadId":"thread_...","turnId":"turn_...","kind":"tool","subject":{"tool":"run_bash","preview":"git push"},"choices":["allow","deny","always_allow"]}}
```

```json
{"jsonrpc":"2.0","id":"server:1","result":{"decision":"allow"}}
```

Structured questions use `userInput/request`; the client replies with
`{"answers":["..."]}`. Disconnect, cancellation, malformed replies, and
unknown decisions fail closed.

## Ownership boundary

The Aivo `AgentEngine` is the only planner/executor for this protocol. Bailey
Cloud may expose app knowledge, task/evidence records, verification helpers,
and bounded Browser/CUA/API operations through MCP. It must not call a model or
run a second planner, recipe loop, retry loop, or completion decision for the
same turn.

Project-scoped MCP consent/configuration, MCP management and interactive OAuth,
attachments, Cloud transport, and edit-review interactions are not advertised
by protocol v1 yet. Turn-scoped model overrides are also not advertised yet;
Desktop applies provider/model changes when it creates the next thread.
