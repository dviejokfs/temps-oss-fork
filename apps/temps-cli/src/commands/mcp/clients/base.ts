import * as fs from 'node:fs'
import * as path from 'node:path'
import * as jsonc from 'jsonc-parser'

export interface McpServerEntry {
  url: string
  apiKey: string
}

export interface InstallResult {
  success: boolean
  alreadyInstalled?: boolean
  reason?: string
}

/** The server name written into every client's config file/registry. */
export const MCP_SERVER_NAME = 'temps'

/** Strips an Authorization header value before it can reach a log, screen, or thrown error. */
export function redactSecrets(text: string): string {
  return text.replace(/Bearer\s+\S+/gi, 'Bearer ***').replace(/Authorization:\s*\S+(\s+\S+)?/gi, 'Authorization: ***')
}

export interface McpClientAdapter {
  readonly id: string
  readonly label: string
  /** Best-effort detection of whether this client is installed on the machine. Advisory only -- add proceeds either way. */
  isClientSupported(): Promise<boolean>
  isServerInstalled(): Promise<boolean>
  /**
   * Best-effort: the connection URL this client currently has configured for
   * the Temps server, or null if not installed / not determinable. `mcp
   * status` parses this to show which tool groups and write mode are active
   * -- see parseMcpUrl in ../groups.ts.
   */
  getServerUrl(): Promise<string | null>
  addServer(entry: McpServerEntry): Promise<InstallResult>
  removeServer(): Promise<InstallResult>
  /** Human-readable description of what `addServer`/`removeServer` will change, for the pre-write confirmation prompt. */
  describeTarget(): Promise<string>
}

/**
 * Default adapter for clients that store MCP servers as a JSON(C) config
 * file with a `<propertyName>.temps` entry. Uses jsonc-parser so an existing
 * file's comments and formatting survive the edit, and diffs the entry
 * before writing so a re-run reports "already installed" instead of
 * rewriting an identical file.
 */
export abstract class JsonConfigMcpClientAdapter implements McpClientAdapter {
  abstract readonly id: string
  abstract readonly label: string

  protected abstract getConfigPath(): string
  protected abstract getServerPropertyName(): string
  protected abstract buildServerConfig(entry: McpServerEntry): Record<string, unknown>
  /** Pulls the connection URL back out of a config entry previously built by buildServerConfig. */
  protected abstract extractUrl(serverConfig: Record<string, unknown>): string | null

  async isClientSupported(): Promise<boolean> {
    return true
  }

  private async readServerConfig(): Promise<Record<string, unknown> | null> {
    const configPath = this.getConfigPath()
    if (!fs.existsSync(configPath)) return null
    try {
      const content = await fs.promises.readFile(configPath, 'utf8')
      const config = jsonc.parse(content) as Record<string, any> | undefined
      const prop = this.getServerPropertyName()
      return config?.[prop]?.[MCP_SERVER_NAME] ?? null
    } catch {
      return null
    }
  }

  async isServerInstalled(): Promise<boolean> {
    return (await this.readServerConfig()) !== null
  }

  async getServerUrl(): Promise<string | null> {
    const serverConfig = await this.readServerConfig()
    return serverConfig ? this.extractUrl(serverConfig) : null
  }

  async addServer(entry: McpServerEntry): Promise<InstallResult> {
    const configPath = this.getConfigPath()
    try {
      await fs.promises.mkdir(path.dirname(configPath), { recursive: true })

      let content = ''
      if (fs.existsSync(configPath)) {
        content = await fs.promises.readFile(configPath, 'utf8')
      }

      const prop = this.getServerPropertyName()
      const existing = (jsonc.parse(content || '{}') ?? {}) as Record<string, any>
      const newEntry = this.buildServerConfig(entry)
      const currentEntry = existing?.[prop]?.[MCP_SERVER_NAME]

      if (currentEntry !== undefined && JSON.stringify(currentEntry) === JSON.stringify(newEntry)) {
        return { success: true, alreadyInstalled: true }
      }

      const edits = jsonc.modify(content, [prop, MCP_SERVER_NAME], newEntry, {
        formattingOptions: { tabSize: 2, insertSpaces: true },
      })
      const updated = jsonc.applyEdits(content, edits)
      // mode: 0o600 -- the file embeds an API key (in an Authorization header
      // value); the default umask-derived mode (typically 0o644) would leave
      // it readable by any other local user on a shared machine.
      await fs.promises.writeFile(configPath, updated, { encoding: 'utf8', mode: 0o600 })
      return { success: true }
    } catch (error) {
      return { success: false, reason: redactSecrets(error instanceof Error ? error.message : String(error)) }
    }
  }

  async removeServer(): Promise<InstallResult> {
    const configPath = this.getConfigPath()
    try {
      if (!fs.existsSync(configPath)) return { success: true, alreadyInstalled: true }

      const content = await fs.promises.readFile(configPath, 'utf8')
      const prop = this.getServerPropertyName()
      const existing = jsonc.parse(content) as Record<string, any> | undefined
      if (!existing?.[prop]?.[MCP_SERVER_NAME]) {
        return { success: true, alreadyInstalled: true }
      }

      const edits = jsonc.modify(content, [prop, MCP_SERVER_NAME], undefined, {
        formattingOptions: { tabSize: 2, insertSpaces: true },
      })
      const updated = jsonc.applyEdits(content, edits)
      await fs.promises.writeFile(configPath, updated, { encoding: 'utf8', mode: 0o600 })
      return { success: true }
    } catch (error) {
      return { success: false, reason: redactSecrets(error instanceof Error ? error.message : String(error)) }
    }
  }

  async describeTarget(): Promise<string> {
    return `${this.getConfigPath()} (${this.getServerPropertyName()}.${MCP_SERVER_NAME})`
  }
}
