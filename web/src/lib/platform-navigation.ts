const PLATFORM_TOOL_PREFIXES = [
  '/tools',
  '/sandboxes',
  '/domains',
  '/certificates',
  '/backups',
  '/email',
  '/ai-gateway',
  '/chat',
  '/ai-workflows',
  '/agent-sandbox',
  '/skills',
  '/mcp-servers',
  '/git-providers',
  '/dns-providers',
  '/proxy',
  '/proxy-logs',
  '/audit-logs',
] as const

export function isPlatformToolsRoute(pathname: string): boolean {
  return PLATFORM_TOOL_PREFIXES.some(
    (prefix) => pathname === prefix || pathname.startsWith(`${prefix}/`)
  )
}
