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
import { getCurrentWindow } from "@tauri-apps/api/window";
import { confirm as confirmDialog, open } from "@tauri-apps/plugin-dialog";
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from "react";

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

type StoredThreadMessage = {
  role: string;
  content: string;
  reasoningContent?: string | null;
};

type PublicProvider = {
  kind: string;
  label: string;
  configurationLocation: "local";
  inferenceLocation: "local" | "remote";
};

type ModelProvider = {
  id: string;
  displayName: string;
  kind: string;
  configurationLocation: "local";
  inferenceLocation: "local" | "remote";
  active: boolean;
  agentCompatible: boolean;
  selectedModel?: string | null;
};

type ProviderListResult = {
  activeModelProvider?: string | null;
  data: ModelProvider[];
};

type ModelListResult = {
  modelProvider: string;
  providerName: string;
  selectedModel?: string | null;
  selectedModelAvailable?: boolean | null;
  catalogAvailable: boolean;
  data: string[];
  warning?: string | null;
};

type ToolSourcesStatus = {
  productTools: {
    configured: boolean;
    connected: boolean;
    tools: number;
    issues: number;
    degraded: boolean;
    approvalRequired: boolean;
  };
  userMcp: {
    scope: "user";
    connectedServers: number;
    tools: number;
    issues: number;
    degraded: boolean;
  };
};

type ThreadSummary = {
  sessionId: string;
  cwd: string;
  provider?: PublicProvider;
  toolSources?: ToolSourcesStatus;
  model: string;
  title: string;
  preview: string;
  updatedAt: string;
  createdAt: string;
};

type ThreadOpenResult = {
  threadId: string;
  sessionId: string;
  cwd: string;
  provider?: PublicProvider;
  toolSources?: ToolSourcesStatus;
  model: string;
  title: string;
  preview?: string;
  updatedAt?: string;
  createdAt?: string;
  messages?: StoredThreadMessage[];
};

type ConversationState = ThreadSummary & {
  threadId?: string;
  items: TranscriptItem[];
  draft: string;
  loaded: boolean;
  hasStarted: boolean;
  durabilityDirty: boolean;
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

const previewConversation: ConversationState = {
  sessionId: "preview-session-initial",
  threadId: "preview-thread-initial",
  cwd: previewProject,
  provider: {
    kind: "aivo_starter",
    label: "Aivo Starter",
    configurationLocation: "local",
    inferenceLocation: "remote",
  },
  toolSources: {
    productTools: {
      configured: true,
      connected: true,
      tools: 24,
      issues: 0,
      degraded: false,
      approvalRequired: true,
    },
    userMcp: {
      scope: "user",
      connectedServers: 0,
      tools: 0,
      issues: 0,
      degraded: false,
    },
  },
  model: "Aivo Starter",
  title: "简化 Bailey 任务界面",
  preview: "布局已经收敛到项目侧栏、当前任务和输入框三层。",
  updatedAt: "2026-07-10T09:00:00.000Z",
  createdAt: "2026-07-10T09:00:00.000Z",
  items: previewItems,
  draft: "",
  loaded: true,
  hasStarted: true,
  durabilityDirty: false,
};

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
  const [newTaskModel, setNewTaskModel] = useState(
    isLayoutPreview ? "Aivo Starter" : "",
  );
  const [providers, setProviders] = useState<ModelProvider[]>(
    isLayoutPreview
      ? [{
          id: "aivo-starter",
          displayName: "Aivo Starter",
          kind: "aivo_starter",
          configurationLocation: "local",
          inferenceLocation: "remote",
          active: true,
          agentCompatible: true,
          selectedModel: "Aivo Starter",
        }]
      : [],
  );
  const [providersLoaded, setProvidersLoaded] = useState(isLayoutPreview);
  const [providerLoadError, setProviderLoadError] = useState("");
  const [newTaskProvider, setNewTaskProvider] = useState(
    isLayoutPreview ? "aivo-starter" : "",
  );
  const [providerDraft, setProviderDraft] = useState("");
  const [modelDraft, setModelDraft] = useState("");
  const [availableModels, setAvailableModels] = useState<string[]>([]);
  const [modelsLoading, setModelsLoading] = useState(false);
  const [modelWarning, setModelWarning] = useState("");
  const [selectedModelAvailable, setSelectedModelAvailable] = useState<boolean | null>(null);
  const [modelMenuOpen, setModelMenuOpen] = useState(false);
  const [threadMenuOpen, setThreadMenuOpen] = useState(false);
  const [searchOpen, setSearchOpen] = useState(false);
  const [searchQuery, setSearchQuery] = useState("");
  const [showStatus, setShowStatus] = useState(false);
  const [conversations, setConversations] = useState<ConversationState[]>(
    isLayoutPreview ? [previewConversation] : [],
  );
  const [activeSessionId, setActiveSessionId] = useState<string | undefined>(
    isLayoutPreview ? previewConversation.sessionId : undefined,
  );
  const [turnId, setTurnId] = useState<string>();
  const [runningThreadId, setRunningThreadId] = useState<string>();
  const [turnStarting, setTurnStarting] = useState(false);
  const [opening, setOpening] = useState(false);
  const [choosingProject, setChoosingProject] = useState(false);
  const [operationError, setOperationError] = useState("");
  const [durabilityRetrying, setDurabilityRetrying] = useState(false);
  const [interaction, setInteraction] = useState<PendingInteraction>();
  const [selectedAnswers, setSelectedAnswers] = useState<string[]>([]);
  const [freeText, setFreeText] = useState("");
  const [diagnostic, setDiagnostic] = useState("");
  const activeConversation = conversations.find(
    (conversation) => conversation.sessionId === activeSessionId,
  );
  const threadId = activeConversation?.threadId;
  const items = activeConversation?.items ?? [];
  const prompt = activeConversation?.draft ?? "";
  const turnState: TurnState = runningThreadId ? "running" : "idle";
  const busy = opening || choosingProject || turnStarting || turnState === "running";
  const scrollRef = useRef<HTMLDivElement>(null);
  const composerRef = useRef<HTMLTextAreaElement>(null);
  const sendButtonRef = useRef<HTMLButtonElement>(null);
  const modelInputRef = useRef<HTMLInputElement>(null);
  const modelRequestRef = useRef(0);
  const modelTriggerRef = useRef<HTMLButtonElement>(null);
  const threadMenuTriggerRef = useRef<HTMLButtonElement>(null);
  const interactionFocusRef = useRef<HTMLButtonElement>(null);
  const freeTextRef = useRef<HTMLInputElement>(null);
  const interactionDialogRef = useRef<HTMLElement>(null);
  const modelPickerRef = useRef<HTMLDivElement>(null);
  const threadMenuRef = useRef<HTMLDivElement>(null);
  const statusAreaRef = useRef<HTMLDivElement>(null);
  const statusTriggerRef = useRef<HTMLButtonElement>(null);
  const searchToggleRef = useRef<HTMLButtonElement>(null);
  const searchInputRef = useRef<HTMLInputElement>(null);
  const openingRef = useRef(false);
  const choosingProjectRef = useRef(false);
  const sendingRef = useRef(false);
  const composingRef = useRef(false);
  const durabilityRetryingRef = useRef(false);
  const allowWindowCloseRef = useRef(false);
  const closeConfirmationPendingRef = useRef(false);
  const focusFrameRef = useRef<number | undefined>(undefined);
  const activeThreadIdRef = useRef<string | undefined>(undefined);
  const activeSessionIdRef = useRef<string | undefined>(undefined);
  const runningThreadIdRef = useRef<string | undefined>(undefined);
  const conversationsRef = useRef<ConversationState[]>([]);
  const runningTurnRef = useRef<{ threadId: string; turnId: string } | undefined>(undefined);

  useLayoutEffect(() => {
    activeThreadIdRef.current = threadId;
    activeSessionIdRef.current = activeSessionId;
    runningThreadIdRef.current = runningThreadId;
    conversationsRef.current = conversations;
  }, [activeSessionId, conversations, runningThreadId, threadId]);

  useEffect(() => () => {
    if (focusFrameRef.current !== undefined) cancelAnimationFrame(focusFrameRef.current);
  }, []);

  useEffect(() => {
    const hasUnsavedConversation = () => conversationsRef.current.some(
      (conversation) => conversation.durabilityDirty,
    );
    const onBeforeUnload = (event: BeforeUnloadEvent) => {
      if (allowWindowCloseRef.current || !hasUnsavedConversation()) return;
      event.preventDefault();
      event.returnValue = "";
    };
    window.addEventListener("beforeunload", onBeforeUnload);

    if (isLayoutPreview) {
      return () => window.removeEventListener("beforeunload", onBeforeUnload);
    }

    const appWindow = getCurrentWindow();
    let disposed = false;
    let unlisten: (() => void) | undefined;
    void appWindow.onCloseRequested(async (event) => {
      if (allowWindowCloseRef.current || !hasUnsavedConversation()) return;
      event.preventDefault();
      if (closeConfirmationPendingRef.current) return;
      closeConfirmationPendingRef.current = true;
      try {
        const discard = await confirmDialog(
          "这次对话还没有保存。现在退出会丢失未保存的内容，仍要退出吗？",
          { title: "退出 Bailey？", kind: "warning" },
        );
        if (discard) {
          allowWindowCloseRef.current = true;
          await appWindow.close();
          return;
        }
        const message = "已取消退出。请先重试保存当前对话。";
        setOperationError(message);
        setStatusText(message);
        setShowStatus(true);
      } catch (error) {
        setDiagnostic(`Could not confirm window close: ${error instanceof Error ? error.message : String(error)}`);
      } finally {
        closeConfirmationPendingRef.current = false;
      }
    }).then((removeListener) => {
      if (disposed) removeListener();
      else unlisten = removeListener;
    });

    return () => {
      disposed = true;
      unlisten?.();
      window.removeEventListener("beforeunload", onBeforeUnload);
    };
  }, []);

  useEffect(() => {
    if (isLayoutPreview) return;
    let cancelled = false;

    client.onEvent = handleEvent;
    client.onDiagnostic = (message) => setDiagnostic(message);
    client.onExit = () => {
      setConnection("stopped");
      setStatusText("Agent Runtime 已停止");
      setRunningThreadId(undefined);
      setTurnId(undefined);
      setTurnStarting(false);
      sendingRef.current = false;
      runningTurnRef.current = undefined;
      setConversations((current) => current.map((conversation) => ({
        ...conversation,
        threadId: undefined,
      })));
      rejectInteraction("Agent runtime exited");
    };
    client.onInteraction = (request) =>
      new Promise((resolve, reject) => {
        const requestThreadId = String(
          (request.params as ApprovalRequest | UserInputRequest).threadId ?? "",
        );
        if (requestThreadId !== activeThreadIdRef.current) {
          reject(new Error("Interaction belongs to an inactive task"));
          return;
        }
        closeAuxiliarySurfaces();
        setInteraction((current) => {
          current?.reject(new Error("Interaction superseded"));
          return { request, resolve, reject };
        });
        setSelectedAnswers([]);
        setFreeText("");
      });
    void client
      .connect()
      .then(async () => {
        const providerLoad = await loadProviders();
        if (cancelled) return;
        setConnection("ready");
        setStatusText("Agent 已就绪");
        const rememberedProject = localStorage.getItem("bailey.cwd");
        if (rememberedProject) {
          void openProject(rememberedProject, providerLoad.preferred, providerLoad.loaded);
        }
      })
      .catch((error: unknown) => {
        if (cancelled) return;
        setConnection("error");
        setStatusText(error instanceof Error ? error.message : String(error));
        setConversations((current) => current.map((conversation) => ({
          ...conversation,
          threadId: undefined,
        })));
        setTurnId(undefined);
        runningTurnRef.current = undefined;
        rejectInteraction("Agent runtime failed to start");
        client.dispose();
      });
    return () => {
      cancelled = true;
      client.dispose();
    };
  }, [client]);

  useEffect(() => {
    scrollRef.current?.scrollTo({
      top: scrollRef.current.scrollHeight,
      behavior: "smooth",
    });
  }, [items.length, interaction]);

  useEffect(() => {
    const textarea = composerRef.current;
    if (!textarea) return;
    textarea.style.height = "58px";
    textarea.style.height = `${Math.min(textarea.scrollHeight, 190)}px`;
  }, [prompt, activeSessionId]);

  useEffect(() => {
    if (
      connection !== "ready"
      || !threadId
      || opening
      || choosingProject
      || turnStarting
      || interaction
      || searchOpen
      || modelMenuOpen
      || threadMenuOpen
      || showStatus
    ) return;
    focusComposer();
  }, [
    activeSessionId,
    choosingProject,
    connection,
    interaction,
    opening,
    threadId,
    turnStarting,
  ]);

  useEffect(() => {
    if (!modelMenuOpen) return;
    cancelComposerFocus();
    const frame = requestAnimationFrame(() => modelInputRef.current?.focus());
    return () => cancelAnimationFrame(frame);
  }, [modelMenuOpen]);

  useEffect(() => {
    if (!interaction) return;
    cancelComposerFocus();
    const frame = requestAnimationFrame(() => {
      (freeTextRef.current ?? interactionFocusRef.current)?.focus();
    });
    return () => cancelAnimationFrame(frame);
  }, [interaction]);

  useEffect(() => {
    if (searchOpen || threadMenuOpen || showStatus) cancelComposerFocus();
  }, [searchOpen, showStatus, threadMenuOpen]);

  useEffect(() => {
    if (!modelMenuOpen && !threadMenuOpen && !showStatus && !searchOpen) return;
    function onPointerDown(event: PointerEvent) {
      const target = event.target as Node;
      if (modelMenuOpen && !modelPickerRef.current?.contains(target)) closeModelMenu(false);
      if (threadMenuOpen && !threadMenuRef.current?.contains(target)) setThreadMenuOpen(false);
      if (showStatus && !statusAreaRef.current?.contains(target)) setShowStatus(false);
      if (
        searchOpen
        && !searchToggleRef.current?.contains(target)
        && !searchInputRef.current?.contains(target)
      ) setSearchOpen(false);
    }
    function onKeyDown(event: KeyboardEvent) {
      if (event.key !== "Escape" || isNativeImeEvent(event, composingRef.current)) return;
      const returnFocus = modelMenuOpen
        ? modelTriggerRef.current
        : threadMenuOpen
          ? threadMenuTriggerRef.current
          : searchOpen
            ? searchToggleRef.current
            : showStatus
              ? statusTriggerRef.current
              : null;
      setModelMenuOpen(false);
      setThreadMenuOpen(false);
      setShowStatus(false);
      setSearchOpen(false);
      if (returnFocus) requestAnimationFrame(() => returnFocus.focus());
    }
    window.addEventListener("pointerdown", onPointerDown);
    window.addEventListener("keydown", onKeyDown);
    return () => {
      window.removeEventListener("pointerdown", onPointerDown);
      window.removeEventListener("keydown", onKeyDown);
    };
  }, [modelMenuOpen, searchOpen, showStatus, threadMenuOpen]);

  function handleEvent(event: AppServerEvent) {
    if (event.threadId !== activeThreadIdRef.current) return;
    const activeTurn = runningTurnRef.current;
    if (
      activeTurn
      && (activeTurn.threadId !== event.threadId || activeTurn.turnId !== event.turnId)
    ) return;
    const payload = event.payload;
    if (event.type === "turn.started") {
      runningTurnRef.current = { threadId: event.threadId, turnId: event.turnId };
      setRunningThreadId(event.threadId);
      setTurnId(event.turnId);
      setStatusText("Bailey 正在工作");
      return;
    }
    if (event.type === "assistant.text.delta") {
      appendStreamingItem(event.threadId, "assistant", String(payload.text ?? ""));
      return;
    }
    if (event.type === "assistant.reasoning.delta") {
      appendStreamingItem(event.threadId, "notice", String(payload.text ?? ""), "思考");
      return;
    }
    if (event.type === "tool.started") {
      appendItem(
        event.threadId,
        "tool",
        pretty(payload.args),
        String(payload.name ?? "工具调用"),
        undefined,
        String(payload.toolCallId ?? ""),
      );
      return;
    }
    if (event.type === "tool.completed") {
      completeToolItem(event.threadId, payload);
      return;
    }
    if (event.type === "plan.updated") {
      appendItem(event.threadId, "notice", pretty(payload.items), "计划已更新");
      return;
    }
    if (event.type === "notice" || event.type === "error") {
      appendItem(
        event.threadId,
        event.type === "error" ? "error" : "notice",
        String(payload.text ?? ""),
      );
      return;
    }
    if (terminalEvents.has(event.type)) {
      const restoreComposerFocus = document.activeElement === sendButtonRef.current;
      const running = runningTurnRef.current;
      if (
        running
        && (running.threadId !== event.threadId || running.turnId !== event.turnId)
      ) return;
      runningTurnRef.current = undefined;
      setRunningThreadId(undefined);
      setTurnId(undefined);
      setTurnStarting(false);
      sendingRef.current = false;
      rejectInteraction(`Turn ended: ${event.type}`);
      if (event.type === "turn.completed") {
        setStatusText("Agent 已就绪");
      } else if (event.type === "turn.cancelled" || event.type === "turn.stopped") {
        setStatusText("任务已停止");
      } else {
        setStatusText("任务失败");
        appendItem(event.threadId, "error", String(payload.error ?? "任务执行失败"));
      }
      if (payload.persisted === false) {
        updateConversationByThread(event.threadId, (conversation) => ({
          ...conversation,
          durabilityDirty: true,
        }));
        appendItem(event.threadId, "error", "任务已结束，但这次对话未能保存；请在运行状态中重试保存。");
        setOperationError("当前对话尚未保存。请先重试保存，成功后才能切换任务。");
        setShowStatus(true);
      } else if (payload.persisted === true) {
        updateConversationByThread(event.threadId, (conversation) => ({
          ...conversation,
          durabilityDirty: false,
        }));
      }
      if (restoreComposerFocus) focusComposer();
    }
  }

  function appendStreamingItem(
    targetThreadId: string,
    kind: Extract<TranscriptItem["kind"], "assistant" | "notice">,
    text: string,
    title?: string,
  ) {
    if (!text) return;
    updateConversationByThread(targetThreadId, (conversation) => {
      const last = conversation.items.at(-1);
      if (last?.kind === kind && last.title === title) {
        return {
          ...conversation,
          items: [
            ...conversation.items.slice(0, -1),
            { ...last, text: `${last.text}${text}` },
          ],
        };
      }
      return { ...conversation, items: [...conversation.items, makeItem(kind, text, title)] };
    });
  }

  function appendItem(
    targetThreadId: string,
    kind: TranscriptItem["kind"],
    text: string,
    title?: string,
    ok?: boolean,
    toolCallId?: string,
  ) {
    if (!text && !title) return;
    updateConversationByThread(targetThreadId, (conversation) => ({
      ...conversation,
      items: [...conversation.items, makeItem(kind, text, title, ok, toolCallId)],
    }));
  }

  function completeToolItem(targetThreadId: string, payload: Record<string, unknown>) {
    const toolCallId = String(payload.toolCallId ?? "");
    const completed = makeItem(
      "tool",
      String(payload.ok ? payload.output ?? "完成" : payload.error ?? "失败"),
      String(payload.name ?? "工具调用"),
      Boolean(payload.ok),
      toolCallId,
    );
    updateConversationByThread(targetThreadId, (conversation) => {
      const current = conversation.items;
      let index = -1;
      if (toolCallId) {
        for (let itemIndex = current.length - 1; itemIndex >= 0; itemIndex -= 1) {
          if (current[itemIndex].toolCallId === toolCallId) {
            index = itemIndex;
            break;
          }
        }
      }
      if (index < 0) return { ...conversation, items: [...current, completed] };
      return {
        ...conversation,
        items: current.map((item, itemIndex) =>
          itemIndex === index ? { ...item, ...completed, id: item.id } : item,
        ),
      };
    });
  }

  async function openProject(
    projectPath: string,
    initialProvider?: ModelProvider,
    providerCatalogLoaded = providersLoaded,
  ) {
    const normalizedPath = projectPath.trim();
    if (!normalizedPath || openingRef.current || sendingRef.current || runningThreadIdRef.current) return;
    if (blockLeavingUnsavedConversation()) return;

    closeAuxiliarySurfaces();
    openingRef.current = true;
    setOpening(true);
    setOperationError("");
    setStatusText("正在打开项目");
    try {
      if (isLayoutPreview) {
        const available = conversationsRef.current
          .filter((conversation) => conversation.cwd === normalizedPath)
          .sort((left, right) => right.updatedAt.localeCompare(left.updatedAt));
        if (available[0]) {
          setCwd(normalizedPath);
          setActiveSessionId(available[0].sessionId);
        } else {
          await createConversation(normalizedPath, initialProvider, providerCatalogLoaded);
        }
      } else {
        const listed = await client.request<{ data: ThreadSummary[] }>("thread/list", {
          cwd: normalizedPath,
        });
        const canonicalPath = listed.data[0]?.cwd ?? normalizedPath;
        rememberProject(canonicalPath);
        if (listed.data.length === 0) {
          await createConversation(canonicalPath, initialProvider, providerCatalogLoaded);
        } else {
          let resumed: ThreadOpenResult | undefined;
          let resumeError: unknown;
          for (const candidate of listed.data) {
            if (
              candidate.sessionId === activeSessionIdRef.current
              && activeThreadIdRef.current
            ) {
              setConversations((current) => mergeThreadSummaries(
                current,
                listed.data,
                canonicalPath,
                candidate.sessionId,
              ));
              setCwd(canonicalPath);
              setStatusText("Agent 已就绪");
              return;
            }
            try {
              resumed = await client.request<ThreadOpenResult>("thread/resume", {
                sessionId: candidate.sessionId,
              });
              break;
            } catch (error) {
              resumeError = error;
            }
          }
          if (!resumed) {
            setConversations((current) => mergeThreadSummaries(
              current,
              listed.data,
              canonicalPath,
            ));
            await createConversation(canonicalPath, initialProvider, providerCatalogLoaded);
            setOperationError("已有历史任务暂时无法恢复，已为这个项目创建一个新任务。");
            setShowStatus(true);
            setDiagnostic(
              `No listed session could be resumed: ${resumeError instanceof Error ? resumeError.message : String(resumeError ?? "unknown")}`,
            );
          } else {
            const previousThreadId = activeThreadIdRef.current;
            await closePreviousRuntime(resumed.threadId);
            const loaded = conversationFromOpenResult(resumed);
            setConversations((current) => {
              const existing = new Map(current.map((conversation) => [conversation.sessionId, conversation]));
              return [
                ...current
                  .map((conversation) => conversation.threadId === previousThreadId
                    ? { ...conversation, threadId: undefined }
                    : conversation)
                  .filter((conversation) => conversation.cwd !== canonicalPath),
                ...listed.data.map((summary) => {
                  if (summary.sessionId === loaded.sessionId) {
                    return {
                      ...loaded,
                      preview: loaded.preview || summary.preview,
                      updatedAt: summary.updatedAt,
                      createdAt: summary.createdAt,
                      draft: existing.get(summary.sessionId)?.draft ?? "",
                      durabilityDirty: existing.get(summary.sessionId)?.durabilityDirty ?? false,
                    };
                  }
                  const previous = existing.get(summary.sessionId);
                  return conversationFromSummary(summary, previous);
                }),
              ];
            });
            setCwd(resumed.cwd);
            setActiveSessionId(resumed.sessionId);
          }
        }
      }
      setTurnId(undefined);
      setRunningThreadId(undefined);
      rejectInteraction("Project changed");
      setStatusText("Agent 已就绪");
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
      || openingRef.current
      || choosingProjectRef.current
      || sendingRef.current
      || runningThreadIdRef.current
    ) return;
    if (blockLeavingUnsavedConversation()) return;

    closeAuxiliarySurfaces();
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
    closeAuxiliarySurfaces();
    if (busy || openingRef.current || sendingRef.current) return;
    if (blockLeavingUnsavedConversation()) return;
    if (!cwd) {
      await chooseProject();
      return;
    }
    openingRef.current = true;
    setOpening(true);
    setOperationError("");
    try {
      await createConversation(cwd);
      setStatusText("Agent 已就绪");
    } catch (error) {
      const message = projectOpenError(error, cwd);
      setOperationError(message);
      setStatusText(message);
      setShowStatus(true);
    } finally {
      openingRef.current = false;
      setOpening(false);
    }
  }

  async function loadProviders(): Promise<{
    preferred?: ModelProvider;
    loaded: boolean;
  }> {
    if (isLayoutPreview) return { preferred: providers[0], loaded: true };
    try {
      const result = await client.request<ProviderListResult>("provider/list");
      const compatible = result.data.filter((provider) => provider.agentCompatible);
      const active = compatible.find(
        (provider) => provider.id === result.activeModelProvider || provider.active,
      );
      const preferred = active ?? compatible[0];
      setProviders(result.data);
      setProvidersLoaded(true);
      setProviderLoadError("");
      setNewTaskProvider((current) => {
        if (compatible.some((provider) => provider.id === current)) return current;
        return preferred?.id ?? "";
      });
      return { preferred, loaded: true };
    } catch (error) {
      setProviders([]);
      setProvidersLoaded(true);
      setProviderLoadError("无法读取模型连接列表，请重试。");
      setDiagnostic(
        `Could not load model providers: ${error instanceof Error ? error.message : String(error)}`,
      );
      return { loaded: true };
    }
  }

  async function retryProviders() {
    const result = await loadProviders();
    const modelProvider = result.preferred?.id ?? "";
    setProviderDraft(modelProvider);
    setModelDraft("");
    setSelectedModelAvailable(null);
    if (modelProvider) await loadModels(modelProvider, true);
  }

  async function loadModels(modelProvider: string, refresh = false) {
    const request = ++modelRequestRef.current;
    setAvailableModels([]);
    setModelWarning("");
    setSelectedModelAvailable(null);
    if (!modelProvider || isLayoutPreview) {
      setModelsLoading(false);
      return;
    }
    setModelsLoading(true);
    try {
      const result = await client.request<ModelListResult>("model/list", {
        modelProvider,
        refresh,
      });
      if (request !== modelRequestRef.current) return;
      setAvailableModels(result.data);
      setSelectedModelAvailable(result.selectedModelAvailable ?? null);
      setProviders((current) => current.map((provider) =>
        provider.id === result.modelProvider
          ? {
              ...provider,
              selectedModel: result.selectedModelAvailable === false
                ? null
                : result.selectedModel,
            }
          : provider,
      ));
      if (result.catalogAvailable && result.selectedModelAvailable === false) {
        setModelWarning("之前保存的默认模型已不在该模型服务中，请选择或填写一个模型。");
      } else {
        setModelWarning(
          result.warning ? "模型服务未提供模型列表，仍可手动填写模型 ID。" : "",
        );
      }
    } catch (error) {
      if (request !== modelRequestRef.current) return;
      setSelectedModelAvailable(null);
      setModelWarning(
        `无法读取模型列表，仍可手动填写：${error instanceof Error ? error.message : String(error)}`,
      );
    } finally {
      if (request === modelRequestRef.current) setModelsLoading(false);
    }
  }

  async function sendTurn() {
    closeAuxiliarySurfaces();
    const text = prompt.trim();
    const sessionId = activeSessionIdRef.current;
    if (
      !sessionId
      || !threadId
      || !text
      || runningThreadIdRef.current
      || openingRef.current
      || sendingRef.current
    ) return;

    const baseItemCount = activeConversation?.items.length ?? 0;
    sendingRef.current = true;
    setTurnStarting(true);
    setOperationError("");

    if (isLayoutPreview) {
      updateConversation(sessionId, (conversation) => ({
        ...conversation,
        title: conversation.hasStarted ? conversation.title : titleFromPrompt(text),
        preview: text,
        updatedAt: new Date().toISOString(),
        draft: "",
        hasStarted: true,
        items: [
          ...conversation.items,
          makeItem("user", text),
          makeItem("notice", "任务已经加入当前线程。", "已接收任务"),
          makeItem("assistant", "我会沿用当前项目上下文继续处理。", "Bailey"),
        ],
      }));
      sendingRef.current = false;
      setTurnStarting(false);
      focusComposer();
      return;
    }

    try {
      const result = await client.request<{ turnId: string }>("turn/start", {
        threadId,
        text,
      });
      updateConversation(sessionId, (conversation) => {
        const insertionIndex = Math.min(baseItemCount, conversation.items.length);
        return {
          ...conversation,
          title: conversation.hasStarted ? conversation.title : titleFromPrompt(text),
          preview: text,
          updatedAt: new Date().toISOString(),
          draft: "",
          hasStarted: true,
          items: [
            ...conversation.items.slice(0, insertionIndex),
            makeItem("user", text),
            ...conversation.items.slice(insertionIndex),
          ],
        };
      });
      setTurnId(result.turnId);
      runningTurnRef.current = { threadId, turnId: result.turnId };
      setRunningThreadId(threadId);
    } catch (error) {
      appendItem(threadId, "error", error instanceof Error ? error.message : String(error));
      setOperationError(error instanceof Error ? error.message : String(error));
    } finally {
      sendingRef.current = false;
      setTurnStarting(false);
      focusComposer();
    }
  }

  async function cancelTurn() {
    if (isLayoutPreview) {
      setRunningThreadId(undefined);
      setTurnId(undefined);
      if (threadId) appendItem(threadId, "notice", "任务已停止。", "已停止");
      return;
    }
    if (!threadId || !turnId) return;
    try {
      await client.request("turn/cancel", { threadId, turnId });
    } catch (error) {
      appendItem(threadId, "error", error instanceof Error ? error.message : String(error));
    }
  }

  async function retryPersistActiveConversation() {
    const active = conversationsRef.current.find(
      (conversation) => conversation.sessionId === activeSessionIdRef.current,
    );
    if (
      !active?.threadId
      || !active.durabilityDirty
      || runningThreadIdRef.current
      || durabilityRetryingRef.current
    ) return;

    durabilityRetryingRef.current = true;
    setDurabilityRetrying(true);
    setOperationError("");
    try {
      const result = await client.request<{ persisted: boolean }>("thread/flush", {
        threadId: active.threadId,
      });
      if (result.persisted !== true) throw new Error("Agent Runtime 未确认保存成功");
      updateConversation(active.sessionId, (conversation) => ({
        ...conversation,
        durabilityDirty: false,
      }));
      appendItem(active.threadId, "notice", "当前对话已经重新保存。", "已保存");
      setStatusText("对话已保存");
      setShowStatus(false);
      focusComposer();
    } catch (error) {
      const message = `重试保存失败：${error instanceof Error ? error.message : String(error)}`;
      setOperationError(message);
      setStatusText(message);
      setShowStatus(true);
    } finally {
      durabilityRetryingRef.current = false;
      setDurabilityRetrying(false);
    }
  }

  async function activateConversation(sessionId: string) {
    if (connection !== "ready") return;
    closeAuxiliarySurfaces();
    if (sessionId === activeSessionIdRef.current) {
      focusComposer();
      return;
    }
    if (
      busy
      || openingRef.current
      || sendingRef.current
    ) return;
    if (blockLeavingUnsavedConversation()) return;
    const target = conversationsRef.current.find((conversation) => conversation.sessionId === sessionId);
    if (!target) return;

    if (isLayoutPreview) {
      setCwd(target.cwd);
      setActiveSessionId(sessionId);
      return;
    }

    openingRef.current = true;
    setOpening(true);
    setOperationError("");
    try {
      const resumed = await client.request<ThreadOpenResult>("thread/resume", { sessionId });
      const previousThreadId = activeThreadIdRef.current;
      await closePreviousRuntime(resumed.threadId);
      const loaded = conversationFromOpenResult(resumed);
      setConversations((current) => current.map((conversation) => {
        if (conversation.threadId === previousThreadId) {
          return { ...conversation, threadId: undefined };
        }
        if (conversation.sessionId === sessionId) {
          return {
            ...loaded,
            preview: loaded.preview || conversation.preview,
            updatedAt: conversation.updatedAt,
            createdAt: conversation.createdAt,
            draft: conversation.draft,
          };
        }
        return conversation;
      }));
      setCwd(resumed.cwd);
      rememberProject(resumed.cwd);
      setActiveSessionId(sessionId);
      setTurnId(undefined);
      rejectInteraction("Task changed");
      setStatusText("Agent 已就绪");
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      setOperationError(message);
      setStatusText(message);
      setShowStatus(true);
    } finally {
      openingRef.current = false;
      setOpening(false);
    }
  }

  async function createConversation(
    projectPath: string,
    initialProvider?: ModelProvider,
    providerCatalogLoaded = providersLoaded,
  ) {
    const now = new Date().toISOString();
    const modelProvider = initialProvider?.id ?? newTaskProvider;
    const selectedProvider = initialProvider
      ?? providers.find((provider) => provider.id === modelProvider);
    const model = newTaskModel.trim();
    if (providerLoadError) {
      throw new Error("无法读取模型连接列表，请先在模型菜单中重试。");
    }
    if (providerCatalogLoaded && !selectedProvider) {
      throw new Error("没有可用于 AgentEngine 的模型连接，请先在 Aivo 中配置模型服务。");
    }
    if (selectedProvider && !model && !selectedProvider.selectedModel?.trim()) {
      throw new Error("这个模型连接没有默认模型，请先选择或填写模型 ID。");
    }
    if (isLayoutPreview) {
      const suffix = `${Date.now()}-${Math.random()}`;
      const conversation: ConversationState = {
        sessionId: `preview-session-${suffix}`,
        threadId: `preview-thread-${suffix}`,
        cwd: projectPath,
        provider: selectedProvider
          ? {
              kind: selectedProvider.kind,
              label: selectedProvider.displayName,
              configurationLocation: selectedProvider.configurationLocation,
              inferenceLocation: selectedProvider.inferenceLocation,
            }
          : undefined,
        model: model || selectedProvider?.selectedModel || "Aivo Starter",
        title: "新任务",
        preview: "",
        updatedAt: now,
        createdAt: now,
        items: [],
        draft: "",
        loaded: true,
        hasStarted: false,
        durabilityDirty: false,
      };
      setConversations((current) => [...current, conversation]);
      setRecentProjects((current) => uniqueProjects([projectPath, ...current]));
      setCwd(projectPath);
      setActiveSessionId(conversation.sessionId);
      return;
    }

    const result = await client.request<ThreadOpenResult>("thread/start", {
      cwd: projectPath,
      ...(modelProvider.trim() ? { modelProvider: modelProvider.trim() } : {}),
      ...(model ? { model } : {}),
    });
    const previousThreadId = activeThreadIdRef.current;
    await closePreviousRuntime(result.threadId, result.sessionId);
    const conversation = conversationFromOpenResult(result);
    setConversations((current) => [
      ...current.map((entry) => entry.threadId === previousThreadId
        ? { ...entry, threadId: undefined }
        : entry),
      conversation,
    ]);
    setCwd(result.cwd);
    rememberProject(result.cwd);
    setActiveSessionId(result.sessionId);
    setTurnId(undefined);
    setRunningThreadId(undefined);
    rejectInteraction("New task created");
  }

  async function closePreviousRuntime(nextThreadId: string, rollbackSessionId?: string) {
    const previousThreadId = activeThreadIdRef.current;
    if (!previousThreadId || previousThreadId === nextThreadId || isLayoutPreview) return;
    try {
      await client.request("thread/close", { threadId: previousThreadId });
    } catch (error) {
      let rollback = "";
      let nextRuntimeClosed = false;
      try {
        await client.request("thread/close", { threadId: nextThreadId });
        nextRuntimeClosed = true;
      } catch (rollbackError) {
        rollback = `; rollback failed: ${rollbackError instanceof Error ? rollbackError.message : String(rollbackError)}`;
      }
      if (nextRuntimeClosed && rollbackSessionId) {
        try {
          await client.request("thread/delete", { sessionId: rollbackSessionId });
        } catch (deleteError) {
          rollback += `; durable rollback failed: ${deleteError instanceof Error ? deleteError.message : String(deleteError)}`;
        }
      }
      const detail = error instanceof Error ? error.message : String(error);
      setDiagnostic(`Could not close previous thread: ${detail}${rollback}`);
      throw new Error(
        rollback
          ? "无法安全切换任务；运行状态可能需要重置，请重新启动 Bailey。"
          : "无法切换任务：当前任务仍在收尾，请稍后重试。",
      );
    }
  }

  function updateConversation(
    sessionId: string,
    updater: (conversation: ConversationState) => ConversationState,
  ) {
    setConversations((current) => current.map((conversation) =>
      conversation.sessionId === sessionId ? updater(conversation) : conversation,
    ));
  }

  function updateConversationByThread(
    targetThreadId: string,
    updater: (conversation: ConversationState) => ConversationState,
  ) {
    setConversations((current) => current.map((conversation) =>
      conversation.threadId === targetThreadId ? updater(conversation) : conversation,
    ));
  }

  function updateDraft(value: string) {
    const sessionId = activeSessionIdRef.current;
    if (!sessionId) return;
    updateConversation(sessionId, (conversation) => ({ ...conversation, draft: value }));
  }

  function focusComposer() {
    cancelComposerFocus();
    focusFrameRef.current = requestAnimationFrame(() => {
      focusFrameRef.current = undefined;
      composerRef.current?.focus({ preventScroll: true });
    });
  }

  function cancelComposerFocus() {
    if (focusFrameRef.current === undefined) return;
    cancelAnimationFrame(focusFrameRef.current);
    focusFrameRef.current = undefined;
  }

  function closeModelMenu(returnFocus: boolean) {
    composingRef.current = false;
    modelRequestRef.current += 1;
    setModelsLoading(false);
    setModelMenuOpen(false);
    if (returnFocus) requestAnimationFrame(() => modelTriggerRef.current?.focus());
  }

  function closeAuxiliarySurfaces() {
    composingRef.current = false;
    modelRequestRef.current += 1;
    setModelsLoading(false);
    setModelMenuOpen(false);
    setThreadMenuOpen(false);
    setShowStatus(false);
    setSearchOpen(false);
  }

  function blockLeavingUnsavedConversation(): boolean {
    const active = conversationsRef.current.find(
      (conversation) => conversation.sessionId === activeSessionIdRef.current,
    );
    if (!active?.durabilityDirty) return false;
    const message = "当前对话尚未保存。请先在运行状态中重试保存，成功后才能切换任务。";
    setOperationError(message);
    setStatusText(message);
    setShowStatus(true);
    return true;
  }

  function handleInteractionKeyDown(event: ReactKeyboardEvent<HTMLElement>) {
    if (isReactImeEvent(event, composingRef.current)) return;
    if (event.key === "Escape") {
      event.preventDefault();
      if (approval) answerInteraction({ decision: "deny" });
      else rejectInteraction("User dismissed input request");
      return;
    }
    if (event.key !== "Tab") return;
    const dialog = interactionDialogRef.current;
    if (!dialog) return;
    const focusable = [...dialog.querySelectorAll<HTMLElement>(
      "button:not([disabled]), input:not([disabled]), textarea:not([disabled]), [tabindex]:not([tabindex='-1'])",
    )].filter((element) => !element.hasAttribute("hidden"));
    if (focusable.length === 0) return;
    const first = focusable[0];
    const last = focusable.at(-1) ?? first;
    if (event.shiftKey && (document.activeElement === first || !dialog.contains(document.activeElement))) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
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
  const currentTaskTitle = activeConversation?.title ?? (threadId ? "新任务" : "选择一个项目");
  const projectTasks = conversations
    .filter((conversation) => conversation.cwd === cwd)
    .sort((left, right) => right.updatedAt.localeCompare(left.updatedAt));
  const visibleProjects = recentProjects.filter((path) =>
    projectName(path).toLocaleLowerCase().includes(searchQuery.toLocaleLowerCase()),
  );
  const connectionText = connectionLabel(connection);
  const statusLabel = operationError ? "需要处理" : connectionText;
  const durabilityDirty = Boolean(activeConversation?.durabilityDirty);
  const newTaskProviderInfo = providers.find((provider) => provider.id === newTaskProvider);
  const newTaskProviderLabel = newTaskProviderInfo?.displayName
    ?? (providerLoadError
      ? "模型连接加载失败"
      : providersLoaded
        ? "未配置模型连接"
        : "正在读取模型连接");
  const newTaskModelLabel = newTaskModel
    || newTaskProviderInfo?.selectedModel
    || "需要选择模型";
  const modelButtonLabel = `${newTaskProviderLabel} · ${newTaskModelLabel} · 新任务`;
  const providerDraftInfo = providers.find((provider) => provider.id === providerDraft);
  const providerDefaultModelAvailable = Boolean(
    providerDraftInfo?.selectedModel?.trim()
      && selectedModelAvailable !== false
      && !modelsLoading,
  );
  const canApplyModelSelection = Boolean(
    providerDraftInfo?.agentCompatible
      && (modelDraft.trim() || providerDefaultModelAvailable),
  );
  const modelLocation = !newTaskProviderInfo
    ? "模型位置待配置"
    : newTaskProviderInfo.inferenceLocation === "local"
      ? "模型本地推理"
      : "模型远端推理";
  const activeProductTools = activeConversation?.toolSources?.productTools;
  const activeUserMcp = activeConversation?.toolSources?.userMcp;
  const activeInferenceLocation = !activeConversation?.provider
    ? "待配置"
    : activeConversation.provider.inferenceLocation === "local"
      ? "本地"
      : "远端";
  const productToolsStatus = !threadId
    ? "等待任务"
    : !activeProductTools
      ? "状态未知"
      : !activeProductTools.configured
        ? "未安装或未配置"
        : activeProductTools.connected && !activeProductTools.degraded
          ? `已连接 · ${activeProductTools.tools} 个工具`
          : `连接异常 · ${activeProductTools.issues} 个问题`;

  return (
    <div className="app-shell">
      <aside className="sidebar" inert={Boolean(interaction)}>
        <div className="sidebar-brand-row">
          <div className="brand-wordmark">
            <strong>Bailey</strong>
            <span>Agent</span>
          </div>
          <button
            ref={searchToggleRef}
            className="icon-button"
            aria-label="搜索项目"
            aria-pressed={searchOpen}
            aria-controls="project-search"
            onClick={() => {
              setModelMenuOpen(false);
              setThreadMenuOpen(false);
              setShowStatus(false);
              setSearchOpen((value) => !value);
            }}
          >
            <MagnifyingGlass size={20} />
          </button>
        </div>

        {searchOpen && (
          <input
            ref={searchInputRef}
            className="project-search"
            id="project-search"
            aria-label="搜索项目"
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
            || busy
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
                || busy
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
                      if (active && threadId) focusComposer();
                      else void openProject(path);
                    }}
                    disabled={connection !== "ready" || busy}
                  >
                    <FolderSimple size={19} weight={active ? "fill" : "regular"} />
                    <span>{projectName(path)}</span>
                  </button>
                  {active && projectTasks.map((conversation) => (
                    <button
                      className={`task-row ${conversation.sessionId === activeSessionId ? "active" : ""}`}
                      aria-current={conversation.sessionId === activeSessionId ? "page" : undefined}
                      onClick={() => void activateConversation(conversation.sessionId)}
                      disabled={connection !== "ready" || busy}
                      key={conversation.sessionId}
                    >
                      <span>{conversation.title}</span>
                    </button>
                  ))}
                </div>
              );
            })}
            {visibleProjects.length === 0 && (
              <button
                className="empty-project-row"
                onClick={() => void chooseProject()}
                disabled={connection !== "ready" || busy}
              >
                <FolderSimplePlus size={18} />
                <span>打开一个项目</span>
              </button>
            )}
          </div>
        </section>

        <div className="account-area" ref={statusAreaRef}>
          {showStatus && (
            <div
              className="status-popover"
              id="runtime-status"
              role="region"
              aria-label="运行状态详情"
              aria-live="polite"
            >
              <strong>{statusLabel}</strong>
              <p>{operationError || (connection === "ready" ? "代码和命令在本机运行" : statusText)}</p>
              {connection === "ready" && (
                <dl className="runtime-boundaries">
                  <div>
                    <dt>Agent / 工具执行</dt>
                    <dd>本地</dd>
                  </div>
                  <div>
                    <dt>模型推理</dt>
                    <dd>{activeInferenceLocation}</dd>
                  </div>
                  <div>
                    <dt>Bailey Local Tools</dt>
                    <dd className={activeProductTools?.degraded ? "degraded" : undefined}>
                      {productToolsStatus}
                    </dd>
                  </div>
                  <div>
                    <dt>用户 MCP</dt>
                    <dd className={activeUserMcp?.degraded ? "degraded" : undefined}>
                      {activeUserMcp
                        ? `${activeUserMcp.connectedServers} 个服务 · ${activeUserMcp.tools} 个工具`
                        : "状态未知"}
                    </dd>
                  </div>
                </dl>
              )}
              {durabilityDirty && (
                <button
                  className="status-action"
                  onClick={() => void retryPersistActiveConversation()}
                  disabled={durabilityRetrying || turnState === "running" || !threadId}
                >
                  {durabilityRetrying && <CircleNotch className="spin" size={14} />}
                  {durabilityRetrying ? "正在保存" : "重试保存"}
                </button>
              )}
              {import.meta.env.DEV && diagnostic && <pre>{diagnostic}</pre>}
            </div>
          )}
          <button
            className="account-row"
            aria-expanded={showStatus}
            aria-controls="runtime-status"
            onClick={() => {
              setModelMenuOpen(false);
              setThreadMenuOpen(false);
              setShowStatus((value) => !value);
            }}
          >
            <span className="account-avatar">B</span>
            <span className="account-name">Bailey</span>
            <span
              className={`status-dot ${operationError ? "error" : connection}`}
              aria-label={statusLabel}
            />
          </button>
          <button
            ref={statusTriggerRef}
            className="icon-button subtle"
            aria-label="运行状态"
            aria-expanded={showStatus}
            aria-controls="runtime-status"
            onClick={() => {
              setModelMenuOpen(false);
              setThreadMenuOpen(false);
              setShowStatus((value) => !value);
            }}
          >
            <Question size={19} />
          </button>
        </div>
      </aside>

      <main className="main-panel">
        <header className="topbar" inert={Boolean(interaction)}>
          <div className="thread-heading">
            <FolderSimple size={21} />
            <h1>{currentTaskTitle}</h1>
            {threadId && (
              <div className="thread-menu-wrap" ref={threadMenuRef}>
                <button
                  ref={threadMenuTriggerRef}
                  className="icon-button subtle"
                  aria-label="任务菜单"
                  aria-expanded={threadMenuOpen}
                  aria-controls="thread-menu"
                  onClick={() => {
                    setModelMenuOpen(false);
                    setShowStatus(false);
                    setThreadMenuOpen((value) => !value);
                  }}
                >
                  <DotsThree size={21} weight="bold" />
                </button>
                {threadMenuOpen && (
                  <div className="thread-menu" id="thread-menu">
                    <button onClick={() => void startNewTask()}>
                      <NotePencil size={17} />
                      新任务
                    </button>
                    <button
                      onClick={() => {
                        setThreadMenuOpen(false);
                        void chooseProject();
                      }}
                    >
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
          <div
            className="transcript-content"
            role="log"
            aria-live="polite"
            inert={Boolean(interaction)}
          >
          {!threadId && (
            <div className="empty-state">
              <h2>开始一个新任务</h2>
              <p>打开项目后，Bailey 会在选定的本地范围内工作。</p>
              <button
                className="primary"
                onClick={() => void chooseProject()}
                disabled={connection !== "ready" || busy}
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
          </div>

          {interaction && (
            <section
              ref={interactionDialogRef}
              className="interaction-card"
              role="dialog"
              aria-modal="true"
              aria-labelledby="interaction-title"
              onKeyDown={handleInteractionKeyDown}
            >
              <div className="interaction-eyebrow">需要你的决定</div>
              {approval && (
                <>
                  <h3 id="interaction-title">允许运行 {approval.subject.tool}？</h3>
                  {approval.subject.preview && <pre>{approval.subject.preview}</pre>}
                  <div className="interaction-actions">
                    <button
                      ref={interactionFocusRef}
                      onClick={() => answerInteraction({ decision: "deny" })}
                    >
                      拒绝
                    </button>
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
                  <h3 id="interaction-title">{userInput.question}</h3>
                  <div className="option-list">
                    {userInput.options.map((option, index) => (
                      <button
                        ref={index === 0 ? interactionFocusRef : undefined}
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
                        ref={freeTextRef}
                        value={freeText}
                        onChange={(event) => setFreeText(event.target.value)}
                        onCompositionStart={() => {
                          composingRef.current = true;
                        }}
                        onCompositionEnd={() => {
                          composingRef.current = false;
                        }}
                        onBlur={() => {
                          composingRef.current = false;
                        }}
                        onKeyDown={(event) => {
                          if (
                            event.key === "Enter"
                            && !isReactImeEvent(event, composingRef.current)
                            && freeText.trim()
                            && !userInput.multiSelect
                          ) {
                            event.preventDefault();
                            answerInteraction({ answers: [freeText.trim()] });
                          }
                        }}
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

        <div className="composer-shell" inert={Boolean(interaction)}>
          <footer className="composer">
            <textarea
              ref={composerRef}
              aria-label="任务内容"
              value={prompt}
              onChange={(event) => updateDraft(event.target.value)}
              onCompositionStart={() => {
                composingRef.current = true;
              }}
              onCompositionEnd={() => {
                composingRef.current = false;
              }}
              onBlur={() => {
                composingRef.current = false;
              }}
              onKeyDown={(event) => {
                const nativeEvent = event.nativeEvent;
                if (
                  event.key === "Enter"
                  && !event.shiftKey
                  && !isNativeImeEvent(nativeEvent, composingRef.current)
                  && !busy
                  && prompt.trim()
                ) {
                  event.preventDefault();
                  void sendTurn();
                }
              }}
              placeholder={
                !threadId
                  ? "先打开一个项目"
                  : turnState === "running"
                    ? "Bailey 正在工作，可先写下一条…"
                    : items.length > 0
                      ? "继续这个任务…"
                      : "告诉 Bailey 要完成什么…"
              }
              disabled={connection !== "ready" || opening || !threadId}
              readOnly={turnStarting}
            />
            <div className="composer-toolbar">
              <span className="local-runtime" aria-live="polite">
                <span className={`status-dot ${connection}`} />
                工具本地执行 · {modelLocation}
              </span>
              <div className="composer-actions">
                <div className="model-picker-wrap" ref={modelPickerRef}>
                  <button
                    ref={modelTriggerRef}
                    className="model-picker"
                    aria-expanded={modelMenuOpen}
                    aria-haspopup="dialog"
                    aria-controls="model-popover"
                    onClick={() => {
                      setThreadMenuOpen(false);
                      setShowStatus(false);
                      if (modelMenuOpen) {
                        closeModelMenu(false);
                        return;
                      }
                      const nextProvider = newTaskProvider
                        || providers.find((provider) => provider.active && provider.agentCompatible)?.id
                        || providers.find((provider) => provider.agentCompatible)?.id
                        || "";
                      setProviderDraft(nextProvider);
                      setModelDraft(newTaskModel);
                      setSelectedModelAvailable(null);
                      setModelMenuOpen(true);
                      void loadModels(nextProvider);
                    }}
                    disabled={connection !== "ready" || opening || turnStarting}
                  >
                    <span>{modelButtonLabel}</span>
                    <CaretDown size={15} />
                  </button>
                  {modelMenuOpen && (
                    <div
                      className="model-popover"
                      id="model-popover"
                      role="dialog"
                      aria-modal="false"
                      aria-label="配置新任务的模型"
                    >
                      {providerLoadError && (
                        <div className="provider-load-error">
                          <span>{providerLoadError}</span>
                          <button onClick={() => void retryProviders()}>重试</button>
                        </div>
                      )}
                      <label htmlFor="model-provider">
                        模型连接（本地配置）
                        <select
                          id="model-provider"
                          value={providerDraft}
                          onChange={(event) => {
                            const modelProvider = event.target.value;
                            setProviderDraft(modelProvider);
                            setModelDraft("");
                            setSelectedModelAvailable(null);
                            void loadModels(modelProvider);
                          }}
                        >
                          {!providerDraft && (
                            <option value="">
                              {providersLoaded ? "选择模型服务" : "Runtime 当前模型连接"}
                            </option>
                          )}
                          {providers.map((provider) => (
                            <option
                              key={provider.id}
                              value={provider.id}
                              disabled={!provider.agentCompatible}
                            >
                              {provider.displayName}{provider.active ? "（当前）" : ""}
                            </option>
                          ))}
                        </select>
                      </label>
                      <label htmlFor="model-id">
                        模型（{providerDraftInfo?.inferenceLocation === "local" ? "本地" : "远端"}）
                        <input
                          ref={modelInputRef}
                          id="model-id"
                          list="available-models"
                          value={modelDraft}
                          onChange={(event) => setModelDraft(event.target.value)}
                          onCompositionStart={() => {
                            composingRef.current = true;
                          }}
                          onCompositionEnd={() => {
                            composingRef.current = false;
                          }}
                          onBlur={() => {
                            composingRef.current = false;
                          }}
                          onKeyDown={(event) => {
                            if (
                              event.key === "Enter"
                              && !isReactImeEvent(event, composingRef.current)
                              && canApplyModelSelection
                            ) {
                              event.preventDefault();
                              setNewTaskProvider(providerDraft);
                              setNewTaskModel(modelDraft.trim());
                              closeModelMenu(true);
                            }
                          }}
                          placeholder={modelsLoading
                            ? "正在读取模型…"
                            : providerDefaultModelAvailable
                              ? `默认：${providerDraftInfo?.selectedModel}`
                              : "填写模型 ID"}
                        />
                        <datalist id="available-models">
                          {availableModels.map((model) => <option key={model} value={model} />)}
                        </datalist>
                      </label>
                      {modelWarning && <p className="model-warning">{modelWarning}</p>}
                      <div className="model-popover-actions">
                        <button onClick={() => closeModelMenu(true)}>取消</button>
                        <button
                          className="primary"
                          disabled={!canApplyModelSelection}
                          onClick={() => {
                            setNewTaskProvider(providerDraft);
                            setNewTaskModel(modelDraft.trim());
                            closeModelMenu(true);
                          }}
                        >
                          应用
                        </button>
                      </div>
                    </div>
                  )}
                </div>
                <button
                  ref={sendButtonRef}
                  className={`send-button ${turnState === "running" ? "stop" : ""}`}
                  onClick={() => turnState === "running" ? void cancelTurn() : void sendTurn()}
                  disabled={turnState === "running"
                    ? !turnId && !isLayoutPreview
                    : connection !== "ready"
                      || opening
                      || turnStarting
                      || !threadId
                      || !prompt.trim()}
                  aria-label={turnState === "running" ? "停止" : turnStarting ? "正在发送" : "发送"}
                >
                  {turnState === "running"
                    ? <Stop size={17} weight="fill" />
                    : turnStarting
                      ? <CircleNotch className="spin" size={18} />
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
    if (!item.title || !item.text) {
      return (
        <div className="activity-row notice static">
          <Brain size={18} />
          <span>{item.title ?? item.text}</span>
        </div>
      );
    }
    return (
      <details className="activity-row notice">
        <summary>
          <Brain size={18} />
          <span>{item.title}</span>
          <CaretDown className="activity-caret" size={15} />
        </summary>
        <pre>{item.text}</pre>
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

function isNativeImeEvent(event: KeyboardEvent, composing: boolean): boolean {
  return composing || event.isComposing || event.keyCode === 229;
}

function isReactImeEvent(
  event: ReactKeyboardEvent<HTMLElement>,
  composing: boolean,
): boolean {
  return isNativeImeEvent(event.nativeEvent, composing);
}

function conversationFromSummary(
  summary: ThreadSummary,
  previous?: ConversationState,
): ConversationState {
  return {
    ...summary,
    toolSources: summary.toolSources ?? previous?.toolSources,
    threadId: undefined,
    items: previous?.items ?? [],
    draft: previous?.draft ?? "",
    loaded: previous?.loaded ?? false,
    hasStarted: previous?.hasStarted ?? Boolean(summary.preview || summary.title !== "新任务"),
    durabilityDirty: previous?.durabilityDirty ?? false,
  };
}

function mergeThreadSummaries(
  current: ConversationState[],
  summaries: ThreadSummary[],
  cwd: string,
  preserveRuntimeSessionId?: string,
): ConversationState[] {
  const existing = new Map(current.map((conversation) => [conversation.sessionId, conversation]));
  return [
    ...current.filter((conversation) => conversation.cwd !== cwd),
    ...summaries.map((summary) => {
      const previous = existing.get(summary.sessionId);
      const conversation = conversationFromSummary(summary, previous);
      if (summary.sessionId !== preserveRuntimeSessionId || !previous?.threadId) return conversation;
      return {
        ...conversation,
        threadId: previous.threadId,
        loaded: true,
      };
    }),
  ];
}

function conversationFromOpenResult(result: ThreadOpenResult): ConversationState {
  const now = new Date().toISOString();
  const messages = result.messages ?? [];
  return {
    sessionId: result.sessionId,
    threadId: result.threadId,
    cwd: result.cwd,
    provider: result.provider,
    toolSources: result.toolSources,
    model: result.model,
    title: result.title || "新任务",
    preview: result.preview ?? messages.at(-1)?.content ?? "",
    updatedAt: result.updatedAt ?? now,
    createdAt: result.createdAt ?? now,
    items: storedMessagesToItems(messages),
    draft: "",
    loaded: true,
    hasStarted: messages.some((message) => message.role === "user")
      || Boolean(result.preview)
      || result.title !== "新任务",
    durabilityDirty: false,
  };
}

function storedMessagesToItems(messages: StoredThreadMessage[]): TranscriptItem[] {
  const items: TranscriptItem[] = [];
  for (const message of messages) {
    if (message.role === "user") {
      items.push(makeItem("user", message.content));
      continue;
    }
    if (message.role !== "assistant") continue;
    if (message.reasoningContent) {
      items.push(makeItem("notice", message.reasoningContent, "思考"));
    }
    if (message.content) items.push(makeItem("assistant", message.content));
  }
  return items;
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
  const characters = [...firstLine];
  return characters.length > 34 ? `${characters.slice(0, 34).join("")}…` : firstLine;
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
