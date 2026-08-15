export interface AiOnboardingCommands {
  installSkills: string
  connectCli: string
}

function normalizeOrigin(origin: string): string {
  return origin.replace(/\/+$/, '')
}

/**
 * Commands shown during AI onboarding. Keeping these in one place prevents
 * the setup page and future onboarding surfaces from drifting onto different
 * package runners or authenticating against the wrong Temps instance.
 */
export function buildAiOnboardingCommands(
  origin: string
): AiOnboardingCommands {
  const instanceOrigin = normalizeOrigin(origin)

  return {
    installSkills: 'bunx skills add gotempsh/temps',
    connectCli: `bunx @temps-sdk/cli login ${instanceOrigin}`,
  }
}
