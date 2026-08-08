import type { ToolCall, ToolCallUpdate } from '@agentclientprotocol/sdk';
import type { TokenState } from '../../types/chat';
import type { Message, NotificationEvent } from '../../types/message';

export type AcpChatStateChange =
  | { type: 'messages'; messages: Message[] }
  | { type: 'tokenState'; tokenState: Partial<TokenState> }
  | { type: 'progressMessage'; message: string | undefined }
  | {
      type: 'sessionInfo';
      name?: string;
      activeRunId?: string | null;
      kajiMode?: string;
    }
  | { type: 'localSteerConfirmed'; messageId: string }
  | { type: 'notification'; notification: NotificationEvent };

export interface AdapterState {
  messages: Message[];
  localSteerTextByMessageId: Map<string, string>;
  toolCallStatesById: Map<string, ToolCallState>;
}

export type ToolCallState = Omit<ToolCallUpdate, '_meta'>;

export interface KajiMessageMeta {
  messageId?: string;
  created?: number;
  outputTokenLimitReached?: boolean;
  fallbackContent?: boolean;
  steer?: boolean;
}

export interface ToolIdentity {
  toolName?: string;
  extensionName?: string;
}

export const DEFAULT_VISIBLE_MESSAGE_METADATA: Message['metadata'] = {
  userVisible: true,
  agentVisible: true,
};

export function messagesChange(state: AdapterState): AcpChatStateChange[] {
  // Pass the live array by reference: the store is the only consumer and it
  // clones on write (applyChatStateChanges). Cloning here as well made every
  // streamed chunk O(messages) twice, which turns session-load replay into
  // O(n^2) on large sessions.
  return [{ type: 'messages', messages: state.messages }];
}

export function cloneMessage(message: Message): Message {
  return {
    ...message,
    content: message.content.map((content) => ({ ...content })),
    metadata: { ...message.metadata },
  };
}

export function getKajiMessageMeta(update: { _meta?: unknown }): KajiMessageMeta {
  if (!isRecord(update._meta)) {
    return {};
  }

  const kaji = update._meta.kaji;
  if (!isRecord(kaji)) {
    return {};
  }

  const outputTokenLimitReached = kaji.outputTokenLimitReached === true;

  return {
    created: typeof kaji.created === 'number' ? kaji.created : undefined,
    messageId: typeof kaji.messageId === 'string' ? kaji.messageId : undefined,
    outputTokenLimitReached: outputTokenLimitReached ? true : undefined,
    fallbackContent: kaji.fallbackContent === true ? true : undefined,
    steer: kaji.steer === true ? true : undefined,
  };
}

export function getKajiActiveRunId(update: { _meta?: unknown }): string | null | undefined {
  if (!isRecord(update._meta)) {
    return undefined;
  }

  const kaji = update._meta.kaji;
  if (!isRecord(kaji) || !('activeRunId' in kaji)) {
    return undefined;
  }

  return typeof kaji.activeRunId === 'string' || kaji.activeRunId === null
    ? kaji.activeRunId
    : undefined;
}

export function getKajiQueuedSteer(update: { _meta?: unknown }): string | undefined {
  if (!isRecord(update._meta)) return undefined;
  const kaji = update._meta.kaji;
  if (!isRecord(kaji) || !isRecord(kaji.queuedSteer)) return undefined;
  return typeof kaji.queuedSteer.messageId === 'string' ? kaji.queuedSteer.messageId : undefined;
}

export function rawInputToArguments(rawInput: unknown): Record<string, unknown> {
  return isRecord(rawInput) ? rawInput : {};
}

export function toolIdentity(update: ToolCall | ToolCallUpdate): ToolIdentity {
  if (!isRecord(update._meta)) {
    return {};
  }

  const kaji = update._meta.kaji;
  if (!isRecord(kaji) || !isRecord(kaji.toolCall)) {
    return {};
  }

  return {
    toolName: typeof kaji.toolCall.toolName === 'string' ? kaji.toolCall.toolName : undefined,
    extensionName:
      typeof kaji.toolCall.extensionName === 'string' ? kaji.toolCall.extensionName : undefined,
  };
}

export function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}
