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
};

type PendingInteraction = {
  request: InteractionRequest;
  resolve: (result: unknown) => void;
  reject: (error: Error) => void;
};

const terminalEvents = new Set([
  "turn.completed",
  "turn.failed",
  "turn.stopped",
  "turn.cancelled",
]);

function App() {
  const client = useMemo(() => new AppServerClient(), []);
  const [connection, setConnection] = useState<ConnectionState>("starting");
  const [statusText, setStatusText] = useState("正在启动 Agent Runtime");
  const [cwd, setCwd] = useState(() => localStorage.getItem("bailey.cwd") ?? "");
  const [model, setModel] = useState("");
  const [keyId, setKeyId] = useState("");
  const [threadId, setThreadId] = useState<string>();
  const [turnId, setTurnId] = useState<string>();
  const [turnState, setTurnState] = useState<TurnState>("idle");
  const [opening, setOpening] = useState(false);
  const [prompt, setPrompt] = useState("");
  const [items, setItems] = useState<TranscriptItem[]>([]);
  const [interaction, setInteraction] = useState<PendingInteraction>();
  const [selectedAnswers, setSelectedAnswers] = useState<string[]>([]);
  const [freeText, setFreeText] = useState("");
  const [diagnostic, setDiagnostic] = useState("");
  const scrollRef = useRef<HTMLDivElement>(null);
  const openingRef = useRef(false);

  useEffect(() => {
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
        setStatusText("Runtime ready");
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
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight, behavior: "smooth" });
  }, [items, interaction]);

  function handleEvent(event: AppServerEvent) {
    const payload = event.payload;
    if (event.type === "turn.started") {
      setTurnState("running");
      setTurnId(event.turnId);
      setStatusText(`Turn ${shortId(event.turnId)} running`);
      return;
    }
    if (event.type === "assistant.text.delta") {
      appendItem("assistant", String(payload.text ?? ""));
      return;
    }
    if (event.type === "assistant.reasoning.delta") {
      appendItem("notice", String(payload.text ?? ""), "思考");
      return;
    }
    if (event.type === "tool.started") {
      appendItem(
        "tool",
        pretty(payload.args),
        `运行 ${String(payload.name ?? "tool")}`,
      );
      return;
    }
    if (event.type === "tool.completed") {
      appendItem(
        "tool",
        String(payload.ok ? payload.output ?? "完成" : payload.error ?? "失败"),
        `${String(payload.name ?? "tool")} ${payload.ok ? "完成" : "失败"}`,
        Boolean(payload.ok),
      );
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
      setStatusText(event.type.replace("turn.", "Turn "));
      if (event.type === "turn.failed") {
        appendItem("error", String(payload.error ?? "Turn failed"));
      }
    }
  }

  function appendItem(
    kind: TranscriptItem["kind"],
    text: string,
    title?: string,
    ok?: boolean,
  ) {
    if (!text) return;
    setItems((current) => [
      ...current,
      { id: `${Date.now()}-${Math.random()}`, kind, text, title, ok },
    ]);
  }

  async function startThread() {
    if (connection !== "ready" || openingRef.current || !cwd.trim()) return;
    openingRef.current = true;
    setOpening(true);
    localStorage.setItem("bailey.cwd", cwd.trim());
    setStatusText("正在打开工作区");
    try {
      const previousThreadId = threadId;
      const result = await client.request<{
        threadId: string;
        model: string;
        keyName: string;
      }>("thread/start", {
        cwd: cwd.trim(),
        ...(model.trim() ? { model: model.trim() } : {}),
        ...(keyId.trim() ? { keyId: keyId.trim() } : {}),
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
      setThreadId(result.threadId);
      setTurnId(undefined);
      setTurnState("idle");
      rejectInteraction("Workspace changed");
      setStatusText(`${result.keyName} · ${result.model}`);
      setItems([]);
    } catch (error) {
      setStatusText(error instanceof Error ? error.message : String(error));
    } finally {
      openingRef.current = false;
      setOpening(false);
    }
  }

  async function sendTurn() {
    const text = prompt.trim();
    if (
      connection !== "ready" ||
      openingRef.current ||
      !threadId ||
      !text ||
      turnState === "running"
    ) return;
    setPrompt("");
    appendItem("user", text);
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
    if (!threadId || !turnId) return;
    try {
      await client.request("turn/cancel", { threadId, turnId });
    } catch (error) {
      appendItem("error", error instanceof Error ? error.message : String(error));
    }
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

  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <div className="brand-mark">B</div>
          <div>
            <strong>Bailey</strong>
            <span>local agent</span>
          </div>
        </div>

        <section className="workspace-card">
          <div className="eyebrow">工作区</div>
          <label>
            项目目录
            <input
              value={cwd}
              onChange={(event) => setCwd(event.target.value)}
              placeholder="/Users/me/project"
              disabled={opening || turnState === "running"}
            />
          </label>
          <label>
            模型（可选）
            <input
              value={model}
              onChange={(event) => setModel(event.target.value)}
              placeholder="使用 Aivo 当前模型"
              disabled={opening || turnState === "running"}
            />
          </label>
          <label>
            Key ID（可选）
            <input
              value={keyId}
              onChange={(event) => setKeyId(event.target.value)}
              placeholder="使用 Aivo 当前 Key"
              disabled={opening || turnState === "running"}
            />
          </label>
          <button
            className="primary wide"
            onClick={() => void startThread()}
            disabled={
              connection !== "ready" ||
              opening ||
              !cwd.trim() ||
              turnState === "running"
            }
          >
            {opening ? "正在打开…" : threadId ? "重新打开" : "打开工作区"}
          </button>
        </section>

        <div className="runtime-status">
          <span className={`status-dot ${connection}`} />
          <div>
            <small>APP SERVER</small>
            <p>{statusText}</p>
          </div>
        </div>
        {diagnostic && <pre className="diagnostic">{diagnostic}</pre>}
      </aside>

      <main className="main-panel">
        <header className="topbar">
          <div>
            <div className="eyebrow">THREAD</div>
            <h1>{threadId ? shortId(threadId) : "选择一个工作区开始"}</h1>
          </div>
          {turnState === "running" && (
            <button className="ghost danger" onClick={() => void cancelTurn()}>
              停止
            </button>
          )}
        </header>

        <div className="transcript" ref={scrollRef}>
          {!threadId && (
            <div className="empty-state">
              <div className="empty-orbit">⌁</div>
              <h2>让 Agent 在你的项目里持续工作</h2>
              <p>界面可以关闭重开；Agent Runtime、审批和工具执行都在独立进程中。</p>
            </div>
          )}
          {threadId && items.length === 0 && (
            <div className="thread-ready">
              <span>READY</span>
              <h2>工作区已连接</h2>
              <p>描述一个具体结果，Bailey 会规划、执行并展示每一步。</p>
            </div>
          )}
          {items.map((item) => (
            <article className={`message ${item.kind}`} key={item.id}>
              <div className="message-label">
                {item.title ?? labelFor(item.kind)}
                {item.kind === "tool" && item.ok !== undefined && (
                  <span className={item.ok ? "ok" : "failed"}>{item.ok ? "OK" : "FAIL"}</span>
                )}
              </div>
              <pre>{item.text}</pre>
            </article>
          ))}

          {interaction && (
            <section className="interaction-card">
              <div className="eyebrow">需要你的决定</div>
              {approval && (
                <>
                  <h3>允许运行 {approval.subject.tool}？</h3>
                  {approval.subject.preview && <pre>{approval.subject.preview}</pre>}
                  <div className="interaction-actions">
                    <button onClick={() => answerInteraction({ decision: "deny" })}>拒绝</button>
                    <button onClick={() => answerInteraction({ decision: "always_allow" })}>
                      本会话总是允许
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
                        aria-pressed={
                          userInput.multiSelect
                            ? selectedAnswers.includes(option.label)
                            : undefined
                        }
                        onClick={() =>
                          userInput.multiSelect
                            ? toggleAnswer(option.label)
                            : answerInteraction({ answers: [option.label] })
                        }
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
                        onClick={() =>
                          answerInteraction({
                            answers: [
                              ...selectedAnswers,
                              ...(freeText.trim() ? [freeText.trim()] : []),
                            ],
                          })
                        }
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
            placeholder={threadId ? "告诉 Bailey 要完成什么…" : "先打开一个工作区"}
            disabled={
              connection !== "ready" || opening || !threadId || turnState === "running"
            }
          />
          <button
            className="send-button"
            onClick={() => void sendTurn()}
            disabled={
              connection !== "ready" ||
              opening ||
              !threadId ||
              !prompt.trim() ||
              turnState === "running"
            }
            aria-label="发送"
          >
            ↑
          </button>
          <div className="composer-hint">Enter 发送 · Shift + Enter 换行</div>
        </footer>
      </main>
    </div>
  );
}

function labelFor(kind: TranscriptItem["kind"]): string {
  return {
    user: "YOU",
    assistant: "BAILEY",
    tool: "TOOL",
    notice: "STATUS",
    error: "ERROR",
  }[kind];
}

function shortId(id: string): string {
  return id.length > 18 ? `${id.slice(0, 12)}…${id.slice(-4)}` : id;
}

function pretty(value: unknown): string {
  if (typeof value === "string") return value;
  return JSON.stringify(value, null, 2);
}

export default App;
