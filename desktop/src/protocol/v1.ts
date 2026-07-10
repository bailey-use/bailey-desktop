export const PROTOCOL_VERSION = 1;

export type RpcId = string | number | null;

export interface RpcRequest {
  jsonrpc: "2.0";
  id: RpcId;
  method: string;
  params?: unknown;
}

export interface RpcResponse {
  jsonrpc: "2.0";
  id: RpcId;
  result?: unknown;
  error?: {
    code: number;
    message: string;
    data?: unknown;
  };
}

export interface RpcNotification {
  jsonrpc: "2.0";
  method: string;
  params?: unknown;
}

export type RpcMessage = RpcRequest | RpcResponse | RpcNotification;

export interface AppServerEvent {
  schemaVersion: 1;
  seq: number;
  threadId: string;
  turnId: string;
  type: string;
  createdAt: string;
  payload: Record<string, unknown>;
}

export interface ApprovalRequest {
  schemaVersion: 1;
  threadId: string;
  turnId: string;
  kind: "tool";
  subject: {
    tool: string;
    preview?: string | null;
  };
  choices: Array<"allow" | "deny" | "always_allow">;
}

export interface UserInputRequest {
  schemaVersion: 1;
  threadId: string;
  turnId: string;
  question: string;
  options: Array<{ label: string; description?: string | null }>;
  allowFreeText: boolean;
  multiSelect: boolean;
}

export type InteractionRequest =
  | { id: RpcId; method: "approval/request"; params: ApprovalRequest }
  | { id: RpcId; method: "userInput/request"; params: UserInputRequest };

export function isRpcResponse(message: RpcMessage): message is RpcResponse {
  return "id" in message && !("method" in message);
}

export function isRpcRequest(message: RpcMessage): message is RpcRequest {
  return "id" in message && "method" in message;
}
