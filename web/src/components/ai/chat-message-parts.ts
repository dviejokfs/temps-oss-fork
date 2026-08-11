import type { PermissionRequest } from './PermissionCard'

/** A tool invocation surfaced over the stream or persisted on the message. */
export interface ToolCall {
  id: string
  name: string
  arguments: string
  /** Undefined while running, a string once done; null only from the API. */
  result?: string | null
}

/** One ordered segment of an assistant turn. */
export type ChatPart =
  | { type: 'text'; text: string }
  | { type: 'tool'; tool: ToolCall }
  | { type: 'permission'; permission: PermissionRequest }

/** Local chat message shape mirroring the generated MessageResponse. */
export interface ChatMessage {
  role: string
  content: string
  created_at?: string
  tools?: ToolCall[]
  parts?: ChatPart[]
}

/**
 * Render segments for an assistant message, with compatibility for persisted
 * turns whose ordered parts contain tool cards but whose prose lives only in
 * the message content column.
 */
export function assistantParts(message: ChatMessage): ChatPart[] {
  if (message.parts && message.parts.length > 0) {
    if (
      message.content &&
      !message.parts.some((part) => part.type === 'text')
    ) {
      return [...message.parts, { type: 'text', text: message.content }]
    }
    return message.parts
  }

  const parts: ChatPart[] = []
  for (const tool of message.tools ?? []) parts.push({ type: 'tool', tool })
  if (message.content) parts.push({ type: 'text', text: message.content })
  return parts
}
