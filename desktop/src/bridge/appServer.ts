import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import {
  PROTOCOL_VERSION,
  isRpcRequest,
  isRpcResponse,
  type AppServerEvent,
  type InteractionRequest,
  type RpcId,
  type RpcMessage,
} from "../protocol/v1";

type Pending = {
  resolve: (value: unknown) => void;
  reject: (error: Error) => void;
};

export class RpcError extends Error {
  constructor(
    message: string,
    readonly code: number,
    readonly data?: unknown,
  ) {
    super(message);
  }
}

export class AppServerClient {
  private nextId = 1;
  private pending = new Map<RpcId, Pending>();
  private unlisten: UnlistenFn[] = [];
  private disposed = false;

  onEvent?: (event: AppServerEvent) => void;
  onInteraction?: (request: InteractionRequest) => Promise<unknown>;
  onDiagnostic?: (message: string) => void;
  onExit?: (payload: unknown) => void;

  async connect(): Promise<void> {
    if (this.disposed) {
      this.cleanup(new Error("Agent runtime client is disposed"));
      throw new Error("Agent runtime client is disposed");
    }
    this.unlisten.push(
      await listen<string>("app-server://message", ({ payload }) => {
        try {
          this.handleMessage(JSON.parse(payload) as RpcMessage);
        } catch (error) {
          this.onDiagnostic?.(
            `Invalid runtime message: ${error instanceof Error ? error.message : String(error)}`,
          );
        }
      }),
      await listen<string>("app-server://stderr", ({ payload }) => {
        this.onDiagnostic?.(payload);
      }),
      await listen("app-server://exit", ({ payload }) => {
        this.rejectAll(new Error("Agent runtime exited"));
        this.onExit?.(payload);
      }),
    );
    if (this.disposed) {
      this.cleanup(new Error("Agent runtime client is disposed"));
      throw new Error("Agent runtime client is disposed");
    }
    await invoke("app_server_start");
    await this.request("initialize", {
      protocolVersion: PROTOCOL_VERSION,
      clientInfo: { name: "bailey-desktop", version: "0.1.0" },
    });
  }

  async request<T>(method: string, params: unknown = {}): Promise<T> {
    if (this.disposed) throw new Error("Agent runtime client is disposed");
    const id = `client:${this.nextId++}`;
    const promise = new Promise<T>((resolve, reject) => {
      this.pending.set(id, {
        resolve: resolve as (value: unknown) => void,
        reject,
      });
    });
    try {
      await this.send({ jsonrpc: "2.0", id, method, params });
    } catch (error) {
      this.pending.delete(id);
      throw error;
    }
    return promise;
  }

  async disconnect(): Promise<void> {
    try {
      await this.request("shutdown");
    } catch {
      // The runtime may already be gone.
    }
    await invoke("app_server_stop");
    this.cleanup(new Error("Agent runtime disconnected"));
  }

  dispose(): void {
    this.cleanup(new Error("Agent runtime disposed"));
    void invoke("app_server_stop").catch(() => {
      // The process may already have exited.
    });
  }

  private async send(message: RpcMessage): Promise<void> {
    await invoke("app_server_send", { message });
  }

  private handleMessage(message: RpcMessage): void {
    if (isRpcResponse(message)) {
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      if (message.error) {
        pending.reject(
          new RpcError(message.error.message, message.error.code, message.error.data),
        );
      } else {
        pending.resolve(message.result);
      }
      return;
    }
    if (isRpcRequest(message)) {
      if (
        message.method !== "approval/request" &&
        message.method !== "userInput/request"
      ) {
        void this.send({
          jsonrpc: "2.0",
          id: message.id,
          error: { code: -32601, message: `Unsupported server method: ${message.method}` },
        });
        return;
      }
      const request = message as InteractionRequest;
      const interaction =
        this.onInteraction?.(request) ?? Promise.reject(new Error("No UI handler"));
      void interaction
        .then(
          (result) => this.send({ jsonrpc: "2.0", id: request.id, result }),
          (error: unknown) => this.send({
            jsonrpc: "2.0",
            id: request.id,
            error: {
              code: -32000,
              message: error instanceof Error ? error.message : String(error),
            },
          }),
        )
        .catch((error: unknown) => {
          this.onDiagnostic?.(
            `Could not reply to runtime: ${error instanceof Error ? error.message : String(error)}`,
          );
        });
      return;
    }
    if (message.method === "event") {
      this.onEvent?.(message.params as AppServerEvent);
    }
  }

  private rejectAll(error: Error): void {
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
  }

  private cleanup(error: Error): void {
    this.disposed = true;
    this.unlisten.splice(0).forEach((unlisten) => unlisten());
    this.rejectAll(error);
    this.onEvent = undefined;
    this.onInteraction = undefined;
    this.onDiagnostic = undefined;
    this.onExit = undefined;
  }
}
