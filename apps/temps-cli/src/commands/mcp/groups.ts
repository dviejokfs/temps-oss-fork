// Tool-group taxonomy for the Temps MCP server, per ADR-039. Mirrors the
// server's `?groups=` query param (crates/temps-mcp-server) — keep in sync
// if the server-side grouping changes.

export interface ToolGroup {
  key: string
  label: string
  categories: string[]
}

export const TOOL_GROUPS: ToolGroup[] = [
  {
    key: 'deployments',
    label: 'Deployments & Projects',
    categories: ['projects', 'deployments', 'environments', 'presets'],
  },
  {
    key: 'infrastructure',
    label: 'Infrastructure',
    categories: ['services', 'containers', 'load-balancer', 'scans'],
  },
  {
    key: 'networking',
    label: 'Networking & Domains',
    categories: ['domains', 'custom-domains', 'dns-providers', 'ip-access'],
  },
  {
    key: 'data',
    label: 'Databases & Backups',
    categories: ['backups', 'dsn'],
  },
  {
    key: 'observability',
    label: 'Observability',
    categories: ['monitors', 'incidents', 'errors', 'proxy-logs', 'funnels', 'analytics'],
  },
  {
    key: 'notifications',
    label: 'Notifications',
    categories: ['notifications', 'notification-prefs', 'webhooks', 'email-domains', 'email-providers'],
  },
  {
    key: 'platform',
    label: 'Platform & Access',
    categories: ['users', 'settings', 'api-keys', 'audit', 'tokens', 'platform'],
  },
]

export function isValidGroupKey(key: string): boolean {
  return TOOL_GROUPS.some((g) => g.key === key)
}

/**
 * Builds the MCP connection URL for a Temps instance. Omits `groups` when
 * every group is selected (server default is "all groups", so the param is
 * only needed to narrow it) and omits `write` when disabled (server default
 * is read-only).
 */
export function buildMcpUrl(apiUrl: string, groups: string[], write: boolean): string {
  const base = apiUrl.replace(/\/+$/, '')
  const url = new URL(`${base}/mcp`)
  if (groups.length > 0 && groups.length < TOOL_GROUPS.length) {
    url.searchParams.set('groups', groups.join(','))
  }
  if (write) {
    url.searchParams.set('write', '1')
  }
  return url.toString()
}
