import { describe, expect, test } from 'bun:test'

import { assistantParts, type ChatMessage } from './chat-message-parts'

const completedTool = {
  id: 'tool-call-1',
  name: 'temps',
  arguments: '{"command":"projects get_projects"}',
  result: '{"status":200}',
}

describe('assistantParts', () => {
  test('keeps persisted assistant text visible when parts contain only tools', () => {
    const message: ChatMessage = {
      role: 'assistant',
      content: 'You have access to one project.',
      parts: [{ type: 'tool', tool: completedTool }],
    }

    expect(assistantParts(message)).toEqual([
      { type: 'tool', tool: completedTool },
      { type: 'text', text: 'You have access to one project.' },
    ])
  })

  test('does not duplicate text already represented in ordered parts', () => {
    const message: ChatMessage = {
      role: 'assistant',
      content: 'Before tool.After tool.',
      parts: [
        { type: 'text', text: 'Before tool.' },
        { type: 'tool', tool: completedTool },
        { type: 'text', text: 'After tool.' },
      ],
    }

    expect(assistantParts(message)).toEqual(message.parts!)
  })
})
