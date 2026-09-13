// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { describe, expect, test } from 'bun:test'
import { renderToStaticMarkup } from 'react-dom/server'
import { MemoryRouter } from 'react-router'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import type { ProviderCatalogDto } from '@/api/client'
import { HarnessSetupCard } from './AgentSandboxProvidersList'
import { ProviderEditor } from './AgentSandboxProviderDetail'
import {
  harnessSetupHref,
  harnessCheckError,
  harnessSetupStatus,
  workspaceReturnTo,
} from './harness-onboarding'

const provider: ProviderCatalogDto = {
  id: 'claude_cli',
  name: 'Claude Code',
  install_command: 'install-cli',
  auth_command: 'login-cli',
  auth_flavors: [],
  models: [],
  runtime_models: [],
  permission_modes: [],
  default_permission_mode_id: 'default',
  credential_saved: false,
  host_authenticated: false,
  model_source: 'bootstrap',
  supports_max_turns: true,
  workspace_ready: false,
}

describe('harness onboarding', () => {
  test('keeps underlying problem, network, and proxy errors visible', () => {
    expect(harnessCheckError({ detail: 'Credential expired' })).toBe(
      'Credential expired'
    )
    expect(harnessCheckError(new TypeError('Failed to fetch'))).toBe(
      'Failed to fetch'
    )
    expect(harnessCheckError('Proxy could not connect')).toBe(
      'Proxy could not connect'
    )
    expect(harnessCheckError(null)).toContain('environment check failed')
  })
  test('does not equate configuration with a verified working connection', () => {
    expect(harnessSetupStatus(provider)).toBe('Not connected')
    expect(
      harnessSetupStatus({ credential_saved: true, workspace_ready: true })
    ).toBe('Credential saved')
    expect(
      harnessSetupStatus({ credential_saved: true, workspace_ready: false })
    ).toBe('Needs attention')
  })

  test('returns to the original workspace and thread after setup', () => {
    const destination =
      '/ai-first?application=app_example&thread=thread_example'
    expect(workspaceReturnTo(destination)).toBe(destination)
    const link = new URL(
      harnessSetupHref('claude_cli', destination),
      'https://temps.invalid'
    )
    expect(link.pathname).toBe('/agent-sandbox/providers/claude_cli')
    expect(link.searchParams.get('returnTo')).toBe(destination)
  })

  test('rejects external, malformed, and unrelated setup return paths', () => {
    for (const value of [
      null,
      '//example.com',
      'https://example.com',
      '/\\example.com',
      '/settings',
      '/workspaces/../../settings',
      '/ai-first\n',
      'javascript:alert(1)',
    ]) {
      expect(workspaceReturnTo(value)).toBe('/ai-first')
    }
    expect(workspaceReturnTo('/workspaces/app_example')).toBe(
      '/workspaces/app_example'
    )
  })

  test('host authentication does not hide missing workspace credentials', () => {
    const html = renderToStaticMarkup(
      <MemoryRouter>
        <HarnessSetupCard
          provider={{
            ...provider,
            host_authenticated: true,
            host_version: '1.2.3',
          }}
          returnTo="/ai-first"
        />
      </MemoryRouter>
    )
    expect(html).toContain('Not connected')
    expect(html).toContain('Authenticated')
    expect(html).toContain('Not saved')
    expect(html).toContain('Connect harness')
    expect(html).not.toContain('Workspace ready')
  })

  test('setup stays renderable without auth methods and explains verification scope', () => {
    const html = renderToStaticMarkup(
      <QueryClientProvider client={new QueryClient()}>
        <MemoryRouter>
          <ProviderEditor provider={provider} isActive={false} />
        </MemoryRouter>
      </QueryClientProvider>
    )
    expect(html).toContain('1. Connect your account')
    expect(html).toContain('2. Choose a model')
    expect(html).toContain('3. Verify your first workspace reply')
    expect(html).toContain('claude setup-token')
    expect(html).toContain('not in the workspace terminal')
    expect(html).toContain(
      'This does not verify a reply in your persistent workspace'
    )
    expect(html).toContain('Advanced: instance default and autofix limits')
  })

  test('OpenCode setup retains the trusted workspace credential disclosure', () => {
    const html = renderToStaticMarkup(
      <QueryClientProvider client={new QueryClient()}>
        <MemoryRouter>
          <ProviderEditor
            provider={{ ...provider, id: 'opencode', name: 'OpenCode' }}
            isActive={false}
          />
        </MemoryRouter>
      </QueryClientProvider>
    )
    expect(html).toContain(
      'Code running as the harness user can access this credential'
    )
    expect(html).not.toContain('reusable credential is never injected')
  })
})
