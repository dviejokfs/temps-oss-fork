import * as fs from 'node:fs'
import * as os from 'node:os'
import * as path from 'node:path'
import { execFileSync } from 'node:child_process'
import { MCP_SERVER_NAME, redactSecrets, type InstallResult, type McpClientAdapter, type McpServerEntry } from './base.js'
import { execErrorMessage, resolveOnPath } from './exec-utils.js'

// Claude Code owns its own config format/location, so this shells out to the
// `claude` CLI (same as PostHog's installer) instead of hand-writing a JSON
// file -- that survives Claude Code changing its config shape underneath us.
export class ClaudeCodeAdapter implements McpClientAdapter {
  readonly id = 'claude-code'
  readonly label = 'Claude Code'
  private binary: string | null | undefined

  private findBinary(): string | null {
    if (this.binary !== undefined) return this.binary
    const candidates = [
      path.join(os.homedir(), '.claude', 'local', 'claude'),
      '/usr/local/bin/claude',
      '/opt/homebrew/bin/claude',
    ]
    for (const candidate of candidates) {
      if (fs.existsSync(candidate)) {
        this.binary = candidate
        return candidate
      }
    }
    this.binary = resolveOnPath('claude')
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
    if (!binary) return { success: false, reason: 'The claude CLI was not found on PATH.' }
    try {
      execFileSync(
        binary,
        [
          'mcp',
          'add',
          '--transport',
          'http',
          '--scope',
          'user',
          MCP_SERVER_NAME,
          entry.url,
          '--header',
          `Authorization: Bearer ${entry.apiKey}`,
        ],
        { stdio: ['ignore', 'pipe', 'pipe'] },
      )
      return { success: true }
    } catch (error) {
      const reason = redactSecrets(execErrorMessage(error))
      if (/already exists/i.test(reason)) return { success: true, alreadyInstalled: true }
      return { success: false, reason }
    }
  }

  async removeServer(): Promise<InstallResult> {
    const binary = this.findBinary()
    if (!binary) return { success: false, reason: 'The claude CLI was not found on PATH.' }
    try {
      execFileSync(binary, ['mcp', 'remove', '--scope', 'user', MCP_SERVER_NAME], {
        stdio: ['ignore', 'pipe', 'pipe'],
      })
      return { success: true }
    } catch (error) {
      const reason = redactSecrets(execErrorMessage(error))
      if (/no such|not found/i.test(reason)) return { success: true, alreadyInstalled: true }
      return { success: false, reason }
    }
  }

  async describeTarget(): Promise<string> {
    return `claude mcp add --transport http --scope user ${MCP_SERVER_NAME} <url>`
  }
}
