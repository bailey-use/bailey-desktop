/// Placeholder loopback URL used during environment injection.
/// The AI launcher replaces this with the actual random port after binding.
pub const PLACEHOLDER_LOOPBACK_URL: &str = "http://127.0.0.1:0";

/// Standard JSON content type header value.
pub const CONTENT_TYPE_JSON: &str = "application/json";

/// Placeholder model value meaning "let the tool use its own default."
pub const MODEL_DEFAULT_PLACEHOLDER: &str = "__default__";

/// Display label shown in the model picker for the default/skip option.
pub const MODEL_DEFAULT_DISPLAY: &str = "(leave it to the tool)";

/// Default provider for new users who have no API keys configured.
/// The sentinel base URL is resolved to the real URL before HTTP calls.
pub const AIVO_STARTER_SENTINEL: &str = "aivo-starter";
pub const AIVO_STARTER_REAL_URL: &str = "https://api.getaivo.dev";
pub const AIVO_STARTER_MODEL: &str = "aivo/starter";
pub const AIVO_STARTER_KEY_NAME: &str = "aivo";
pub const AIVO_STARTER_EMPTY_SECRET: &str = "";

/// Bailey Desktop's first-party OpenAI-compatible model gateway. This is a
/// normal provider URL stored in Aivo's local provider store; it is not a
/// second agent or an in-process service. Desktop deployments may override the
/// URL/model through `BAILEY_CLOUD_MODEL_BASE_URL` and `BAILEY_CLOUD_MODEL`.
pub const BAILEY_CLOUD_MODEL_BASE_URL: &str = "https://bailey.meidaquan.com/v1";
pub const BAILEY_CLOUD_RECORD_BASE_URL: &str = "https://bailey.meidaquan.com/api";
pub const BAILEY_CLOUD_PROVIDER_NAME: &str = "Bailey Cloud";
pub const BAILEY_CLOUD_DEFAULT_MODEL: &str = "bailey/default";

/// Base URL of the aivo web app (account sign-in + device-link approval).
/// `aivo login` hits its `/api/device/*` endpoints and opens its `/device`
/// page. Distinct from the API gateway (`AIVO_STARTER_REAL_URL`). Overridable
/// via the `AIVO_WEBSITE_BASE_URL` env var for testing against `wrangler pages dev`.
pub const AIVO_WEBSITE_BASE_URL: &str = "https://getaivo.dev";

/// AI tool names recognized as positional arguments to `aivo run` and as the
/// first token of a Bundle alias's launch line (e.g. `aivo alias quick claude
/// --key work`). Also doubles as the top-level shortcut list (`aivo claude
/// ...` → `aivo run claude ...`).
pub const KNOWN_TOOLS: &[&str] = &["claude", "codex", "codex-app", "gemini", "opencode", "pi"];

/// Names a user must not register as an alias because they collide with
/// built-in commands or shortcuts and would shadow `aivo <name>` / `aivo run
/// <name>` dispatch. Includes top-level subcommands, the `ls` info alias, the
/// shortcut keywords (`use`, `ping`), and the known tool names.
pub const RESERVED_ALIAS_NAMES: &[&str] = &[
    // Top-level subcommands
    "app-server",
    "run",
    "keys",
    "account",
    "usage",
    "code",
    "chat",
    "models",
    "serve",
    "alias",
    "info",
    "ls",
    "login",
    "logout",
    "logs",
    "stats",
    "update",
    "context",
    "mcp",
    "mcp-serve",
    "skills",
    "share",
    "hf",
    "plugins",
    "plugin",
    // Shortcut keywords rewritten in `rewrite_cli_args`
    "use",
    "ping",
    // Action keywords shared across keys/logs/alias/plugins/hf
    "list",
    "rm",
    "remove",
    // AI tools (also rewritten as shortcuts)
    "claude",
    "codex",
    "codex-app",
    "gemini",
    "opencode",
    "pi",
];
