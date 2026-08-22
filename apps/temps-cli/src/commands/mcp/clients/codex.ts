import { execFileSync } from 'node:child_process'
import { MCP_SERVER_NAME, redactSecrets, type InstallResult, type McpClientAdapter, type McpServerEntry } from './base.js'
import { execErrorMessage, resolveOnPath } from './exec-utils.js'

const TOKEN_ENV_VAR = 'TEMPS_MCP_AUTH_HEADER'

// Same rationale as Claude Code: shell out to the official `codex` CLI so
// Codex's own config format (config.toml) is never hand-written here.
export class CodexAdapter implements McpClientAdapter {
  readonly id = 'codex'
  readonly label = 'Codex'
  private binary: string | null | undefined

  private findBinary(): string | null {
    if (this.binary !== undefined) return this.binary
    this.binary = resolveOnPath('codex')
    return this.binary
  }

  async isClientSupported(): Promise<boolean> {
    return this.findBinary() !== null
  }

  async isServerInstalled(): Promise<boolean> {
    const binary = this.findBinary()
    if (!binary) return false
    try {
      const out = execFileSync(binary, ['mcp', 'list'], { stdio: ['ignore', 'pipe', 'pipe'] })
        .toString()
        .toLowerCase()
      return out.includes(MCP_SERVER_NAME)
    } catch {
      return false
    }
  }

  async addServer(entry: McpServerEntry): Promise<InstallResult> {
    const binary = this.findBinary()
    if (!binary) return { success: false, reason: 'The codex CLI was not found on PATH.' }
    try {
      execFileSync(
        binary,
        ['mcp', 'add', MCP_SERVER_NAME, '--url', entry.url, '--bearer-token-env-var', TOKEN_ENV_VAR],
        {
          stdio: ['ignore', 'pipe', 'pipe'],
          env: { ...process.env, [TOKEN_ENV_VAR]: `Bearer ${entry.apiKey}` },
        },
      )
      return { success: true }
    } catch (error) {
      const reason = redactSecrets(execErrorMessage(error))
      if (/already (installed|exists|added|registered)/i.test(reason)) {
        return { success: true, alreadyInstalled: true }
      }
      return { success: false, reason }
    }
  }

  async removeServer(): Promise<InstallResult> {
    const binary = this.findBinary()
    if (!binary) return { success: false, reason: 'The codex CLI was not found on PATH.' }
    try {
      execFileSync(binary, ['mcp', 'remove', MCP_SERVER_NAME], { stdio: ['ignore', 'pipe', 'pipe'] })
      return { success: true }
    } catch (error) {
      const reason = redactSecrets(execErrorMessage(error))
      if (/not found|no such/i.test(reason)) return { success: true, alreadyInstalled: true }
      return { success: false, reason }
    }
  }

  async describeTarget(): Promise<string> {
    return `codex mcp add ${MCP_SERVER_NAME} --url <url> --bearer-token-env-var ${TOKEN_ENV_VAR}`
  }
}
