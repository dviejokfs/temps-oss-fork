import * as os from 'node:os'
import * as path from 'node:path'
import { JsonConfigMcpClientAdapter, type McpServerEntry } from './base.js'

// Claude Desktop only speaks stdio MCP, so the entry bridges through the
// `mcp-remote` npm package (spawned via npx) rather than connecting to the
// HTTP endpoint directly. Requires Node.js on the user's machine.
export class ClaudeDesktopAdapter extends JsonConfigMcpClientAdapter {
  readonly id = 'claude-desktop'
  readonly label = 'Claude Desktop'

  protected getConfigPath(): string {
    const home = os.homedir()
    if (process.platform === 'darwin') {
      return path.join(home, 'Library', 'Application Support', 'Claude', 'claude_desktop_config.json')
    }
    if (process.platform === 'win32') {
      return path.join(process.env.APPDATA || '', 'Claude', 'claude_desktop_config.json')
    }
    throw new Error('Claude Desktop is only available on macOS and Windows')
  }

  protected getServerPropertyName(): string {
    return 'mcpServers'
  }

  protected buildServerConfig(entry: McpServerEntry): Record<string, unknown> {
    return {
      command: 'npx',
      args: ['-y', 'mcp-remote@latest', entry.url, '--header', `Authorization:Bearer ${entry.apiKey}`],
    }
  }

  override async isClientSupported(): Promise<boolean> {
    return process.platform === 'darwin' || process.platform === 'win32'
  }
}
