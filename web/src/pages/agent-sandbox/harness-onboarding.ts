// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import type { ProviderCatalogDto } from '@/api/client'
import { problemDetail } from '@/lib/api-problem'

export function harnessCheckError(error: unknown): string {
  const fallback =
    'The environment check failed. Check the saved credential and retry.'
  return problemDetail(
    error,
    typeof error === 'string' && error.trim()
      ? error
      : error instanceof Error && error.message
        ? error.message
        : fallback
  )
}

export function harnessSetupStatus(
  provider: Pick<ProviderCatalogDto, 'credential_saved' | 'workspace_ready'>
) {
  if (!provider.credential_saved) return 'Not connected'
  return provider.workspace_ready ? 'Credential saved' : 'Needs attention'
}

export function workspaceReturnTo(value: string | null): string {
  if (
    !value ||
    !value.startsWith('/') ||
    value.startsWith('//') ||
    /[\\\s]/.test(value)
  )
    return '/ai-first'
  const url = new URL(value, 'https://temps.invalid')
  if (url.origin !== 'https://temps.invalid') return '/ai-first'
  if (
    url.pathname !== '/ai-first' &&
    url.pathname !== '/workspaces' &&
    !/^\/workspaces\/[a-zA-Z0-9_-]+$/.test(url.pathname)
  )
    return '/ai-first'
  return `${url.pathname}${url.search}`
}

export function harnessSetupHref(providerId: string | null, returnTo: string) {
  const path =
    '/agent-sandbox/providers' +
    (providerId ? `/${encodeURIComponent(providerId)}` : '')
  return `${path}?returnTo=${encodeURIComponent(workspaceReturnTo(returnTo))}`
}
