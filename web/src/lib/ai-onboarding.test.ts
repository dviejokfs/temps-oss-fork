import { describe, expect, test } from 'bun:test'
import {
  AI_HARNESS_KEY_NAME,
  buildAiHarnessCommands,
  buildAiHarnessKeyHref,
} from './ai-onboarding'

describe('AI harness onboarding', () => {
  test('installs only the canonical Temps skill', () => {
    expect(
      buildAiHarnessCommands('https://temps.example.com').installSkill
    ).toBe('bunx skills add gotempsh/temps --skill temps')
  })

  test('connects the pinned CLI without putting the API key in command text', () => {
    const commands = buildAiHarnessCommands('https://temps.example.com/')

    expect(commands.connectCli).toContain(
      'bunx @temps-sdk/cli@0.1.33 login https://temps.example.com'
    )
    expect(commands.connectCli).toContain('--api-key "$TEMPS_API_KEY"')
    expect(commands.connectCli).toContain('--context temps-example-com')
    expect(commands.connectCli).toContain('unset TEMPS_API_KEY')
  })

  test('verifies the same explicit target context', () => {
    const commands = buildAiHarnessCommands('http://localhost:3003')

    expect(commands.verifyIdentity).toBe(
      'bunx @temps-sdk/cli@0.1.33 --target-context localhost whoami'
    )
    expect(commands.verifyProjects).toContain(
      '--target-context localhost projects list --json'
    )
  })

  test('deep-links to an admin key dedicated to the harness', () => {
    const href = buildAiHarnessKeyHref()
    const url = new URL(href, 'https://temps.example.com')

    expect(url.pathname).toBe('/settings/keys/new')
    expect(url.searchParams.get('name')).toBe(AI_HARNESS_KEY_NAME)
    expect(url.searchParams.get('role')).toBe('admin')
    expect(url.searchParams.get('returnTo')).toBe('/setup/ai')
  })
})
