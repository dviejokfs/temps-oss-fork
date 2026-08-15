import { describe, expect, test } from 'bun:test'
import { buildAiOnboardingCommands } from './ai-onboarding'

describe('AI onboarding commands', () => {
  test('installs Temps skills with bunx', () => {
    expect(buildAiOnboardingCommands('https://temps.example.com')).toEqual({
      installSkills: 'bunx skills add gotempsh/temps',
      connectCli: 'bunx @temps-sdk/cli login https://temps.example.com',
    })
  })

  test('does not duplicate a trailing slash in the instance login command', () => {
    expect(
      buildAiOnboardingCommands('https://temps.example.com/').connectCli
    ).toBe('bunx @temps-sdk/cli login https://temps.example.com')
  })
})
