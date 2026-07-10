import {
  ArrowUp,
  Brain,
  CaretDown,
  CheckCircle,
  CircleNotch,
  DotsThree,
  FolderSimple,
  FolderSimplePlus,
  MagnifyingGlass,
  NotePencil,
  Question,
  Stop,
  WarningCircle,
  Wrench,
} from "@phosphor-icons/react";
import { open } from "@tauri-apps/plugin-dialog";
import { useEffect, useMemo, useRef, useState } from "react";

import { AppServerClient } from "./bridge/appServer";
import type {
  AppServerEvent,
  ApprovalRequest,
  InteractionRequest,
  UserInputRequest,
} from "./protocol/v1";

type ConnectionState = "starting" | "ready" | "error" | "stopped";
type TurnState = "idle" | "running";

type TranscriptItem = {
  id: string;
  kind: "user" | "assistant" | "tool" | "notice" | "error";
  title?: string;
  text: string;
  ok?: boolean;
  toolCallId?: string;
};

type PendingInteraction = {
  request: InteractionRequest;
  resolve: (result: unknown) => void;
  reject: (error: Error) => void;
};

const RECENT_PROJECTS_KEY = "bailey.recentProjects";
const isLayoutPreview = import.meta.env.DEV
  && new URLSearchParams(window.location.search).has("layoutPreview");
const previewProject = "/Users/bailey/bailey-desktop";

const terminalEvents = new Set([
  "turn.completed",
  "turn.failed",
  "turn.stopped",
  "turn.cancelled",
]);

const previewItems: TranscriptItem[] = [
  {
    id: "preview-user",
    kind: "user",
    text: "把 Bailey 的任务界面调整得更简洁，保留现有 Agent 流程。",
  },
  {
    id: "preview-notice",
    kind: "notice",
    title: "已分析界面结构",
    text: "读取了前端布局和运行状态。",
  },
  {
    id: "preview-tool",
    kind: "tool",
    title: "检查前端构建",
    text: "pnpm check\nTypeScript 检查通过",
    ok: true,
  },
  {
    id: "preview-assistant",
    kind: "assistant",
    text: "布局已经收敛到项目侧栏、当前任务和输入框三层，目录和运行时细节不会出现在主界面。",
  },
];

function App() {
  const client = useMemo(() => new AppServerClient(), []);
  const [connection, setConnection] = useState<ConnectionState>(
    isLayoutPreview ? "ready" : "starting",
  );
  const [statusText, setStatusText] = useState(
    isLayoutPreview ? "Agent 已就绪" : "正在启动 Agent Runtime",
  );
  const [cwd, setCwd] = useState(() =>
    isLayoutPreview ? previewProject : localStorage.getItem("bailey.cwd") ?? "",
  );
  const [recentProjects, setRecentProjects] = useState(() =>
    isLayoutPreview ? [previewProject] : readRecentProjects(),
  );
  const [model, setModel] = useState(isLayoutPreview ? "Aivo Starter" : "");
  const [activeModel, setActiveModel] = useState(
    isLayoutPreview ? "Aivo Starter" : "",
  );
  const [modelDraft, setModelDraft] = useState("");
  const [modelMenuOpen, setModelMenuOpen] = useState(false);
  const [threadMenuOpen, setThreadMenuOpen] = useState(false);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [showStatus, setShowStatus] = useState(false);
  const [threadId, setThreadId] = useState<string | undefined>(
    isLayoutPreview ? "preview-thread" : undefined,
  );
  const [turnId, setTurnId] = useState<string>();
  const [turnState, setTurnState] = useState<TurnState>("idle");
  const [opening, setOpening] = useState(false);
  const [choosingProject, setChoosingProject] = useState(false);
  const [operationError, setOperationError] = useState("");
  const [prompt, setPrompt] = useState("");
  const [taskTitle, setTaskTitle] = useState<string | undefined>(
    isLayoutPreview ? "简化 Bailey 任务界面" : undefined,
  );
  const [items, setItems] = useState<TranscriptItem[]>(
    isLayoutPreview ? previewItems : [],
  );
  const [interaction, setInteraction] = useState<PendingInteraction>();
  const [selectedAnswers, setSelectedAnswers] = useState<string[]>([]);
  const [freeText, setFreeText] = useState("");
  const [diagnostic, setDiagnostic] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const openingRef = useRef(false);
  const choosingProjectRef = useRef(false);

  useEffect(() => {
    if (isLayoutPreview) return;

    client.onEvent = handleEvent;
    client.onDiagnostic = (message) => setDiagnostic(message);
    client.onExit = () => {
      setConnection("stopped");
      setStatusText("Agent Runtime 已停止");
      setTurnState("idle");
      setTurnId(undefined);
      setThreadId(undefined);
      rejectInteraction("Agent runtime exited");
    };
    client.onInteraction = (request) =>
      new Promise((resolve, reject) => {
        setInteraction((current) => {
          current?.reject(new Error("Interaction superseded"));
          return { request, resolve, reject };
        });
        setSelectedAnswers([]);
        setFreeText("");
      });
    void client
      .connect()
      .then(() => {
        setConnection("ready");
        setStatusText("Agent 已就绪");
        const rememberedProject = localStorage.getItem("bailey.cwd");
        if (rememberedProject) void openProject(rememberedProject);
      })
      .catch((error: unknown) => {
        setConnection("error");
        setStatusText(error instanceof Error ? error.message : String(error));
        setThreadId(undefined);
        setTurnId(undefined);
        rejectInteraction("Agent runtime failed to start");
        client.dispose();
      });
    return () => {
      client.dispose();
    };
  }, [client]);

  useEffect(() => {
    scrollRef.current?.scrollTo({
      top: scrollRef.current.scrollHeight,
      behavior: "smooth",
    });
  }, [items, interaction]);

  function handleEvent(event: AppServerEvent) {
    const payload = event.payload;
    if (event.type === "turn.started") {
      setTurnState("running");
      setTurnId(event.turnId);
      setStatusText("Bailey 正在工作");
      return;
    }
    if (event.type === "assistant.text.delta") {
      appendStreamingItem("assistant", String(payload.text ?? ""));
      return;
    }
    if (event.type === "assistant.reasoning.delta") {
      appendStreamingItem("notice", String(payload.text ?? ""), "思考");
      return;
    }
    if (event.type === "tool.started") {
      appendItem(
        "tool",
        pretty(payload.args),
        String(payload.name ?? "工具调用"),
        undefined,
        String(payload.toolCallId ?? ""),
      );
      return;
    }
    if (event.type === "tool.completed") {
      completeToolItem(payload);
      return;
    }
    if (event.type === "plan.updated") {
      appendItem("notice", pretty(payload.items), "计划已更新");
      return;
    }
    if (event.type === "notice" || event.type === "error") {
      appendItem(
        event.type === "error" ? "error" : "notice",
        String(payload.text ?? ""),
      );
      return;
    }
    if (terminalEvents.has(event.type)) {
      setTurnState("idle");
      setTurnId(undefined);
      rejectInteraction(`Turn ended: ${event.type}`);
      if (event.type === "turn.completed") {
        setStatusText("Agent 已就绪");
      } else if (event.type === "turn.cancelled" || event.type === "turn.stopped") {
        setStatusText("任务已停止");
      } else {
        setStatusText("任务失败");
        appendItem("error", String(payload.error ?? "任务执行失败"));
      }
    }
  }

  function appendStreamingItem(
    kind: Extract<TranscriptItem["kind"], "assistant" | "notice">,
    text: string,
    title?: string,
  ) {
    if (!text) return;
    setItems((current) => {
      const last = current.at(-1);
      if (last?.kind === kind && last.title === title) {
        return [
          ...current.slice(0, -1),
          { ...last, text: `${last.text}${text}` },
        ];
      }
      return [...current, makeItem(kind, text, title)];
    });
  }

  function appendItem(
    kind: TranscriptItem["kind"],
    text: string,
    title?: string,
    ok?: boolean,
    toolCallId?: string,
  ) {
    if (!text && !title) return;
    setItems((current) => [
      ...current,
      makeItem(kind, text, title, ok, toolCallId),
    ]);
  }

  function completeToolItem(payload: Record<string, unknown>) {
    const toolCallId = String(payload.toolCallId ?? "");
    const completed = makeItem(
      "tool",
      String(payload.ok ? payload.output ?? "完成" : payload.error ?? "失败"),
      String(payload.name ?? "工具调用"),
      Boolean(payload.ok),
      toolCallId,
    );
    setItems((current) => {
      let index = -1;
      if (toolCallId) {
        for (let itemIndex = current.length - 1; itemIndex >= 0; itemIndex -= 1) {
          if (current[itemIndex].toolCallId === toolCallId) {
            index = itemIndex;
            break;
          }
        }
      }
      if (index < 0) return [...current, completed];
      return current.map((item, itemIndex) =>
        itemIndex === index ? { ...item, ...completed, id: item.id } : item,
      );
    });
  }

  async function openProject(projectPath: string) {
    const normalizedPath = projectPath.trim();
    if (!normalizedPath || openingRef.current) return;

    if (isLayoutPreview) {
      setCwd(normalizedPath);
      setRecentProjects((current) => uniqueProjects([normalizedPath, ...current]));
      setThreadId(`preview-${Date.now()}`);
      setTaskTitle(undefined);
      setItems([]);
      setPrompt("");
      setActiveModel(model || "Aivo Starter");
      return;
    }

    openingRef.current = true;
    setOpening(true);
    setOperationError("");
    setStatusText("正在打开项目");
    try {
      const previousThreadId = threadId;
      const result = await client.request<{
        threadId: string;
        cwd: string;
        model: string;
      }>("thread/start", {
        cwd: normalizedPath,
        ...(model.trim() ? { model: model.trim() } : {}),
      });
      if (previousThreadId) {
        try {
          await client.request("thread/close", { threadId: previousThreadId });
        } catch (error) {
          setDiagnostic(
            `Could not close previous thread: ${error instanceof Error ? error.message : String(error)}`,
          );
        }
      }
      setCwd(result.cwd);
      rememberProject(result.cwd);
      setThreadId(result.threadId);
      setTurnId(undefined);
      setTurnState("idle");
      rejectInteraction("Project changed");
      setStatusText("Agent 已就绪");
      setTaskTitle(undefined);
      setItems([]);
      setPrompt("");
      setActiveModel(result.model);
    } catch (error) {
      const message = projectOpenError(error, normalizedPath);
      setStatusText(message);
      setOperationError(message);
      setShowStatus(true);
    } finally {
      openingRef.current = false;
      setOpening(false);
    }
  }

  async function chooseProject() {
    if (
      connection !== "ready"
      || opening
      || choosingProjectRef.current
      || turnState === "running"
    ) return;

    if (isLayoutPreview) {
      await openProject(previewProject);
      return;
    }

    choosingProjectRef.current = true;
    setChoosingProject(true);
    setOperationError("");
    try {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "打开项目",
      });
      if (typeof selected === "string") await openProject(selected);
    } catch (error) {
      const message = `无法打开目录选择器：${error instanceof Error ? error.message : String(error)}`;
      setStatusText(message);
      setOperationError(message);
      setShowStatus(true);
    } finally {
      choosingProjectRef.current = false;
      setChoosingProject(false);
    }
  }

  async function startNewTask() {
    setThreadMenuOpen(false);
    if (turnState === "running" || opening || choosingProject) return;
    if (!cwd) {
      await chooseProject();
      return;
    }
    await openProject(cwd);
  }

  async function sendTurn() {
    const text = prompt.trim();
    if (!threadId || !text || turnState === "running" || openingRef.current) return;

    setPrompt("");
    if (!taskTitle) setTaskTitle(titleFromPrompt(text));
    appendItem("user", text);

    if (isLayoutPreview) {
      appendItem("notice", "任务已经加入当前线程。", "已接收任务");
      appendItem("assistant", "我会沿用当前项目上下文继续处理。", "Bailey");
      return;
    }

    try {
      const result = await client.request<{ turnId: string }>("turn/start", {
        threadId,
        text,
      });
      setTurnId(result.turnId);
      setTurnState("running");
    } catch (error) {
      appendItem("error", error instanceof Error ? error.message : String(error));
    }
  }

  async function cancelTurn() {
    if (isLayoutPreview) {
      setTurnState("idle");
      setTurnId(undefined);
      appendItem("notice", "任务已停止。", "已停止");
      return;
    }
    if (!threadId || !turnId) return;
    try {
      await client.request("turn/cancel", { threadId, turnId });
    } catch (error) {
      appendItem("error", error instanceof Error ? error.message : String(error));
    }
  }

  function rememberProject(path: string) {
    localStorage.setItem("bailey.cwd", path);
    setRecentProjects((current) => {
      const next = uniqueProjects([path, ...current]).slice(0, 12);
      localStorage.setItem(RECENT_PROJECTS_KEY, JSON.stringify(next));
      return next;
    });
  }

  function answerInteraction(result: unknown) {
    interaction?.resolve(result);
    setInteraction(undefined);
    setSelectedAnswers([]);
    setFreeText("");
  }

  function rejectInteraction(message: string) {
    setInteraction((current) => {
      current?.reject(new Error(message));
      return undefined;
    });
    setSelectedAnswers([]);
    setFreeText("");
  }

  function toggleAnswer(label: string) {
    setSelectedAnswers((current) =>
      current.includes(label)
        ? current.filter((answer) => answer !== label)
        : [...current, label],
    );
  }

  const approval = interaction?.request.method === "approval/request"
    ? (interaction.request.params as ApprovalRequest)
    : undefined;
  const userInput = interaction?.request.method === "userInput/request"
    ? (interaction.request.params as UserInputRequest)
    : undefined;
  const currentProjectName = cwd ? projectName(cwd) : "";
  const currentTaskTitle = taskTitle ?? (threadId ? "新任务" : "选择一个项目");
  const visibleProjects = recentProjects.filter((path) =>
    projectName(path).toLocaleLowerCase().includes(searchQuery.toLocaleLowerCase()),
  );
  const connectionText = connectionLabel(connection);
  const statusLabel = operationError ? "需要处理" : connectionText;

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="sidebar-brand-row">
          <div className="brand-wordmark">
            <strong>Bailey</strong>
            <span>Agent</span>
          </div>
          <button
            className="icon-button"
            aria-label="搜索项目"
            aria-pressed={searchOpen}
            onClick={() => setSearchOpen((value) => !value)}
          >
            <MagnifyingGlass size={20} />
          </button>
        </div>

        {searchOpen && (
          <input
            className="project-search"
            value={searchQuery}
            onChange={(event) => setSearchQuery(event.target.value)}
            placeholder="搜索项目"
            autoFocus
          />
        )}

        <button
          className="sidebar-action"
          onClick={() => void startNewTask()}
          disabled={
            connection !== "ready"
            || opening
            || choosingProject
            || turnState === "running"
          }
        >
          {opening || choosingProject
            ? <CircleNotch className="spin" size={20} />
            : <NotePencil size={20} />}
          <span>{opening || choosingProject ? "正在打开" : "新任务"}</span>
        </button>

        <section className="projects-section" aria-label="项目">
          <div className="section-heading">
            <span>Projects</span>
            <button
              className="icon-button subtle"
              aria-label="打开项目"
              onClick={() => void chooseProject()}
              disabled={
                connection !== "ready"
                || opening
                || choosingProject
                || turnState === "running"
              }
            >
              <FolderSimplePlus size={18} />
            </button>
          </div>

          <div className="project-list">
            {visibleProjects.map((path) => {
              const active = path === cwd;
              return (
                <div className="project-group" key={path}>
                  <button
                    className={`project-row ${active ? "active" : ""}`}
                    aria-current={active ? "page" : undefined}
                    onClick={() => {
                      if (!active || !threadId) void openProject(path);
                    }}
                    disabled={opening || choosingProject || turnState === "running"}
                  >
                    <FolderSimple size={19} weight={active ? "fill" : "regular"} />
                    <span>{projectName(path)}</span>
                  </button>
                  {active && threadId && (
                    <button className="task-row active" aria-current="page">
                      <span>{currentTaskTitle}</span>
                    </button>
                  )}
                </div>
              );
            })}
            {visibleProjects.length === 0 && (
              <button
                className="empty-project-row"
                onClick={() => void chooseProject()}
                disabled={connection !== "ready" || opening || choosingProject}
              >
                <FolderSimplePlus size={18} />
                <span>打开一个项目</span>
              </button>
            )}
          </div>
        </section>

        <div className="account-area">
          {showStatus && (
            <div className="status-popover">
              <strong>{statusLabel}</strong>
              <p>{operationError || (connection === "ready" ? "代码和命令在本机运行" : statusText)}</p>
              {import.meta.env.DEV && diagnostic && <pre>{diagnostic}</pre>}
            </div>
          )}
          <button className="account-row" onClick={() => setShowStatus((value) => !value)}>
            <span className="account-avatar">B</span>
            <span className="account-name">Bailey</span>
            <span
              className={`status-dot ${operationError ? "error" : connection}`}
              aria-label={statusLabel}
            />
          </button>
          <button
            className="icon-button subtle"
            aria-label="运行状态"
            onClick={() => setShowStatus((value) => !value)}
          >
            <Question size={19} />
          </button>
        </div>
      </aside>

      <main className="main-panel">
        <header className="topbar">
          <div className="thread-heading">
            <FolderSimple size={21} />
            <h1>{currentTaskTitle}</h1>
            {threadId && (
              <div className="thread-menu-wrap">
                <button
                  className="icon-button subtle"
                  aria-label="任务菜单"
                  aria-expanded={threadMenuOpen}
                  onClick={() => setThreadMenuOpen((value) => !value)}
                >
                  <DotsThree size={21} weight="bold" />
                </button>
                {threadMenuOpen && (
                  <div className="thread-menu">
                    <button onClick={() => void startNewTask()}>
                      <NotePencil size={17} />
                      新任务
                    </button>
                    <button onClick={() => void chooseProject()}>
                      <FolderSimplePlus size={17} />
                      打开项目
                    </button>
                  </div>
                )}
              </div>
            )}
          </div>
          {turnState === "running" && <span className="working-label">Bailey 正在工作</span>}
        </header>

        <div className="transcript" ref={scrollRef}>
          {!threadId && (
            <div className="empty-state">
              <h2>开始一个新任务</h2>
              <p>打开项目后，Bailey 会在选定的本地范围内工作。</p>
              <button
                className="primary"
                onClick={() => void chooseProject()}
                disabled={connection !== "ready" || opening || choosingProject}
              >
                <FolderSimplePlus size={18} />
                打开项目
              </button>
            </div>
          )}

          {threadId && items.length === 0 && (
            <div className="thread-ready">
              <h2>{currentProjectName ? `在 ${currentProjectName} 中开始` : "开始一个新任务"}</h2>
              <p>描述你希望完成的结果，Bailey 会在需要时请求确认。</p>
            </div>
          )}

          {items.map((item) => (
            <MessageItem item={item} key={item.id} />
          ))}

          {interaction && (
            <section className="interaction-card">
              <div className="interaction-eyebrow">需要你的决定</div>
              {approval && (
                <>
                  <h3>允许运行 {approval.subject.tool}？</h3>
                  {approval.subject.preview && <pre>{approval.subject.preview}</pre>}
                  <div className="interaction-actions">
                    <button onClick={() => answerInteraction({ decision: "deny" })}>拒绝</button>
                    <button onClick={() => answerInteraction({ decision: "always_allow" })}>
                      本任务总是允许
                    </button>
                    <button className="primary" onClick={() => answerInteraction({ decision: "allow" })}>
                      允许一次
                    </button>
                  </div>
                </>
              )}
              {userInput && (
                <>
                  <h3>{userInput.question}</h3>
                  <div className="option-list">
                    {userInput.options.map((option) => (
                      <button
                        key={option.label}
                        className={selectedAnswers.includes(option.label) ? "selected" : undefined}
                        aria-pressed={userInput.multiSelect
                          ? selectedAnswers.includes(option.label)
                          : undefined}
                        onClick={() => userInput.multiSelect
                          ? toggleAnswer(option.label)
                          : answerInteraction({ answers: [option.label] })}
                      >
                        <strong>{option.label}</strong>
                        {option.description && <span>{option.description}</span>}
                      </button>
                    ))}
                  </div>
                  {userInput.allowFreeText && (
                    <div className="free-input">
                      <input
                        value={freeText}
                        onChange={(event) => setFreeText(event.target.value)}
                        placeholder="输入其他答案"
                      />
                      {!userInput.multiSelect && (
                        <button
                          className="primary"
                          onClick={() => answerInteraction({ answers: [freeText.trim()] })}
                          disabled={!freeText.trim()}
                        >
                          回答
                        </button>
                      )}
                    </div>
                  )}
                  {userInput.multiSelect && (
                    <div className="interaction-actions multi-select-actions">
                      <button
                        className="primary"
                        onClick={() => answerInteraction({
                          answers: [
                            ...selectedAnswers,
                            ...(freeText.trim() ? [freeText.trim()] : []),
                          ],
                        })}
                        disabled={selectedAnswers.length === 0 && !freeText.trim()}
                      >
                        提交选择
                      </button>
                    </div>
                  )}
                </>
              )}
            </section>
          )}
        </div>

        <div className="composer-shell">
          <footer className="composer">
            <textarea
              value={prompt}
              onChange={(event) => setPrompt(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && !event.shiftKey) {
                  event.preventDefault();
                  void sendTurn();
                }
              }}
              placeholder={
                !threadId
                  ? "先打开一个项目"
                  : turnState === "running"
                    ? "Bailey 正在工作…"
                    : items.length > 0
                      ? "继续这个任务…"
                      : "告诉 Bailey 要完成什么…"
              }
              disabled={connection !== "ready" || opening || !threadId || turnState === "running"}
            />
            <div className="composer-toolbar">
              <span className="local-runtime">
                <span className={`status-dot ${connection}`} />
                本地执行
              </span>
              <div className="composer-actions">
                <div className="model-picker-wrap">
                  <button
                    className="model-picker"
                    aria-expanded={modelMenuOpen}
                    onClick={() => {
                      setModelDraft(model || activeModel);
                      setModelMenuOpen((value) => !value);
                    }}
                    disabled={opening || turnState === "running"}
                  >
                    <span>{activeModel || model || "当前模型"}</span>
                    <CaretDown size={15} />
                  </button>
                  {modelMenuOpen && (
                    <div className="model-popover">
                      <label htmlFor="model-id">
                        下个新任务使用的模型
                        <input
                          id="model-id"
                          value={modelDraft}
                          onChange={(event) => setModelDraft(event.target.value)}
                          placeholder="使用 Aivo 当前模型"
                        />
                      </label>
                      <div className="model-popover-actions">
                        <button onClick={() => setModelMenuOpen(false)}>取消</button>
                        <button
                          className="primary"
                          onClick={() => {
                            setModel(modelDraft.trim());
                            setModelMenuOpen(false);
                          }}
                        >
                          应用
                        </button>
                      </div>
                    </div>
                  )}
                </div>
                <button
                  className={`send-button ${turnState === "running" ? "stop" : ""}`}
                  onClick={() => turnState === "running" ? void cancelTurn() : void sendTurn()}
                  disabled={turnState === "running"
                    ? !turnId && !isLayoutPreview
                    : connection !== "ready" || opening || !threadId || !prompt.trim()}
                  aria-label={turnState === "running" ? "停止" : "发送"}
                >
                  {turnState === "running"
                    ? <Stop size={17} weight="fill" />
                    : <ArrowUp size={19} weight="bold" />}
                </button>
              </div>
            </div>
          </footer>
        </div>
      </main>
    </div>
  );
}

function MessageItem({ item }: { item: TranscriptItem }) {
  if (item.kind === "tool") {
    return (
      <details className={`activity-row tool ${item.ok === false ? "failed" : ""}`}>
        <summary>
          <Wrench size={18} />
          <span>{item.title ?? "工具调用"}</span>
          {item.ok === true && <CheckCircle size={16} weight="fill" />}
          {item.ok === false && <WarningCircle size={16} weight="fill" />}
          <CaretDown className="activity-caret" size={15} />
        </summary>
        {item.text && <pre>{item.text}</pre>}
      </details>
    );
  }

  if (item.kind === "notice") {
    return (
      <details className="activity-row notice">
        <summary>
          <Brain size={18} />
          <span>{item.title ?? item.text}</span>
          {item.title && item.text && <CaretDown className="activity-caret" size={15} />}
        </summary>
        {item.title && item.text && <pre>{item.text}</pre>}
      </details>
    );
  }

  if (item.kind === "user") {
    return (
      <article className="message user">
        <p>{item.text}</p>
      </article>
    );
  }

  if (item.kind === "error") {
    return (
      <article className="message error">
        <div className="message-author"><WarningCircle size={18} />错误</div>
        <pre>{item.text}</pre>
      </article>
    );
  }

  return (
    <article className="message assistant">
      <div className="message-author">Bailey</div>
      <pre>{item.text}</pre>
    </article>
  );
}

function makeItem(
  kind: TranscriptItem["kind"],
  text: string,
  title?: string,
  ok?: boolean,
  toolCallId?: string,
): TranscriptItem {
  return {
    id: crypto.randomUUID(),
    kind,
    text,
    title,
    ok,
    toolCallId: toolCallId || undefined,
  };
}

function readRecentProjects(): string[] {
  let stored: string[] = [];
  try {
    const parsed = JSON.parse(localStorage.getItem(RECENT_PROJECTS_KEY) ?? "[]");
    if (Array.isArray(parsed)) stored = parsed.filter((value): value is string => typeof value === "string");
  } catch {
    // Ignore corrupt local UI preferences.
  }
  const legacy = localStorage.getItem("bailey.cwd");
  return uniqueProjects([...(legacy ? [legacy] : []), ...stored]);
}

function uniqueProjects(projects: string[]): string[] {
  return [...new Set(projects.map((path) => path.trim()).filter(Boolean))];
}

function projectName(path: string): string {
  const withoutTrailingSeparators = path.replace(/[\\/]+$/, "");
  const segments = withoutTrailingSeparators.split(/[\\/]/).filter(Boolean);
  return segments.at(-1) ?? path;
}

function titleFromPrompt(text: string): string {
  const firstLine = text.split(/\r?\n/, 1)[0].trim();
  return firstLine.length > 34 ? `${firstLine.slice(0, 34)}…` : firstLine;
}

function projectOpenError(error: unknown, path: string): string {
  const raw = error instanceof Error ? error.message : String(error);
  const name = projectName(path);
  const detail = raw.split(path).join(name);
  return `无法打开 ${name}：${detail}`;
}

function connectionLabel(connection: ConnectionState): string {
  return {
    starting: "正在启动",
    ready: "Agent 已就绪",
    error: "连接失败",
    stopped: "Agent 已停止",
  }[connection];
}

function pretty(value: unknown): string {
  if (typeof value === "string") return value;
  return JSON.stringify(value, null, 2);
}

export default App;
