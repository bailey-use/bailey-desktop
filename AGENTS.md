# Bailey Desktop Agent Instructions

## Runtime boundary

- Keep this repository an upstream-tracking Aivo fork. Preserve the Aivo CLI
  and built-in Code functionality; attach Bailey behavior at App Server,
  provider, MCP, and Desktop UI boundaries whenever possible.
- Aivo `AgentEngine` is the only planner and executor for a Desktop turn.
  Built-in code tools and Bailey Local Tools belong to the same engine/tool
  loop.
- Treat the Bailey model gateway as an Aivo model provider. It may route,
  authenticate, inject Bailey knowledge, meter, and fail over model requests,
  but it is not an agent.
- Bailey Local Tools are product-managed local capabilities, not entries users
  must copy into their MCP configuration. Keep user MCP as a separate extension
  surface and never load project MCP in App Server without an explicit consent
  flow.
- Bailey Cloud is ordinary model-gateway, account, sync, knowledge, evidence,
  and record infrastructure. Do not add a Cloud MCP agent, second model client,
  planner, recipe loop, autonomous retry loop, or completion decision there.
- Provider secrets, base URLs, and protocol-routing details stay inside Aivo's
  local provider store and must not cross the Desktop protocol.

The canonical architecture is
`../bailey-use/docs/layered-entry-architecture.md`.

## Test execution permission

Do not run tests, builds, checks, formatters, development servers, sidecars,
installers, browser launchers, or browser automation unless the user explicitly
asks for that execution in the current request. Reading, reviewing, planning,
or editing is not permission to execute them.

Static source and diff inspection is allowed. When execution is not authorized,
report that verification was limited to static review.

No ordinary unit test may open a browser tab, launch a desktop app, contact a
real adapter/provider, or depend on a developer's local environment. Real
browser, network, sidecar, and packaged-app checks must be separate opt-in
integration/acceptance paths and still require explicit user permission.
