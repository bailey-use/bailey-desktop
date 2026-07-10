# Aivo App Server Protocol v1

`aivo app-server --stdio` exposes the in-process `AgentEngine` to desktop and
IDE clients. It is a long-lived process; the GUI is a client, not the owner of
the agent loop.

## Transport

- UTF-8 JSON-RPC 2.0 over stdin/stdout.
- Exactly one JSON object per line.
- stdout is protocol-only. Diagnostics go to stderr.
- The maximum inbound frame is 1 MiB.
- API secrets are resolved from Aivo's local key store and never cross the
  protocol.

The client starts with:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientInfo":{"name":"bailey-desktop","version":"0.1.0"}}}
```

The server accepts exactly protocol version `1` and reports its current
capabilities. `health/check` is the only method allowed before initialization.

## Client methods

### `health/check`

Returns `starting`, `ready`, or `draining`, the process id, protocol version,
and in-memory thread count.

### `thread/start`

```json
{"jsonrpc":"2.0","id":2,"method":"thread/start","params":{"cwd":"/work/project","keyId":"optional-local-key-id","model":"optional-model-id"}}
```

`keyId` and `model` fall back to Aivo's active key and selected coding model.
The first version keeps threads in memory and allows one active turn per
thread.

### `thread/close`

```json
{"jsonrpc":"2.0","id":3,"method":"thread/close","params":{"threadId":"thread_..."}}
```

Stops any active turn, releases background jobs and removes the in-memory
thread. Desktop clients close the previous thread after a replacement has been
created successfully.

### `turn/start`

```json
{"jsonrpc":"2.0","id":4,"method":"turn/start","params":{"threadId":"thread_...","text":"Fix the failing test"}}
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

Cancellation is idempotent. It stops model/tool work and denies pending
interactions, but does not claim to roll back side effects that already
happened.

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

The Aivo `AgentEngine` is the only planner/executor for this protocol. A Bailey
Cloud integration may authenticate devices, assign remote tasks, map Cloud task
ids to local thread/turn ids, and record events. It must not run a second
planner or recipe loop for the same turn.

Persistence, resume/replay, MCP consent, attachments, Cloud transport, and
edit-review interactions are intentionally not advertised by protocol v1 yet.
