import type { Command } from 'commander'
import { createApiKey } from '../../api/sdk.gen.js'
import { getApiUrl, requireAuth } from '../../config/store.js'
import { client, getErrorMessage, setupClient } from '../../lib/api-client.js'
import { header, icons, info, keyValue, newline, success, warning } from '../../ui/output.js'
import { promptCheckbox, promptConfirm, promptSelect } from '../../ui/prompts.js'
import { withSpinner } from '../../ui/spinner.js'
import { printTable, type TableColumn } from '../../ui/table.js'
import { CLIENT_ADAPTERS, getClientAdapter, listClientIds } from './clients/index.js'
import { buildMcpUrl, isValidGroupKey, TOOL_GROUPS } from './groups.js'
import { probeMcpEndpoint } from './probe.js'

interface AddOptions {
  groups?: string
  write?: boolean
  yes?: boolean
  apiKey?: string
}

export function registerMcpCommands(program: Command): void {
  const mcp = program
    .command('mcp')
    .description('Configure this Temps instance as an MCP server for AI clients (Claude Code, Claude Desktop, Codex, Cursor, VS Code, Windsurf, Zed)')

  mcp
    .command('add [client]')
    .description(`Configure an AI client to connect to this Temps instance over MCP. Clients: ${listClientIds().join(', ')}`)
    .option('-g, --groups <groups>', 'Comma-separated tool groups to enable (default: all)')
    .option('-w, --write', 'Enable write tools (deploy, delete, restart, etc). Default: read-only')
    .option('-k, --api-key <key>', 'Use this API key instead of creating or prompting for one')
    .option('-y, --yes', 'Skip prompts and confirmation (uses defaults; requires --api-key or an existing login)')
    .action(addAction)

  mcp
    .command('remove [client]')
    .description('Remove the Temps MCP server from an AI client')
    .action(removeAction)

  mcp
    .command('status')
    .description('Show which AI clients have the Temps MCP server configured')
    .action(statusAction)
}

async function addAction(clientArg: string | undefined, options: AddOptions): Promise<void> {
  await requireAuth()
  await setupClient()

  const clientId =
    clientArg ||
    (await promptSelect({
      message: 'Which AI client do you want to configure?',
      choices: CLIENT_ADAPTERS.map((c) => ({ name: c.label, value: c.id })),
    }))

  const adapter = getClientAdapter(clientId)
  if (!adapter) {
    warning(`Unknown client "${clientId}". Available: ${listClientIds().join(', ')}`)
    return
  }

  const apiUrl = getApiUrl()

  const probe = await withSpinner('Checking for Temps MCP support...', () => probeMcpEndpoint(apiUrl))
  if (!probe.supported) {
    warning(`This Temps instance does not expose an MCP endpoint (${probe.reason}).`)
    info('MCP support ships behind a feature flag -- ask an admin to enable it, or upgrade this instance.')
    return
  }

  const clientSupported = await adapter.isClientSupported()
  if (!clientSupported) {
    warning(`${adapter.label} was not detected on this machine -- continuing anyway.`)
  }

  let groups: string[]
  if (options.groups) {
    groups = options.groups
      .split(',')
      .map((g) => g.trim())
      .filter(Boolean)
    const invalid = groups.filter((g) => !isValidGroupKey(g))
    if (invalid.length > 0) {
      warning(`Unknown tool group(s): ${invalid.join(', ')}. Available: ${TOOL_GROUPS.map((g) => g.key).join(', ')}`)
      return
    }
  } else if (options.yes) {
    groups = TOOL_GROUPS.map((g) => g.key)
  } else {
    groups = await promptCheckbox({
      message: 'Which tool groups should be available?',
      choices: TOOL_GROUPS.map((g) => ({
        name: `${g.label} (${g.categories.length} categories)`,
        value: g.key,
      })),
    })
    if (groups.length === 0) groups = TOOL_GROUPS.map((g) => g.key)
  }

  const write =
    options.write ??
    (options.yes
      ? false
      : await promptConfirm({
          message:
            'Allow write tools (deploy, delete, restart, etc)? A Temps instance controls real infrastructure -- ' +
            'even with this on, every write requires an explicit confirmation in the AI client before it executes.',
          default: false,
        }))

  let apiKey = options.apiKey
  if (!apiKey) {
    const wantsNewKey = options.yes
      ? false
      : await promptConfirm({
          message: 'Create a new dedicated API key scoped for MCP access? (recommended over reusing an existing key)',
          default: true,
        })

    if (wantsNewKey) {
      const created = await withSpinner('Creating a scoped API key...', async () => {
        const { data, error } = await createApiKey({
          client,
          body: {
            name: `mcp-${adapter.id}-${new Date().toISOString().slice(0, 10)}`,
            role_type: write ? 'user' : 'reader',
            expires_at: null,
            permissions: null,
          },
        })
        if (error) throw new Error(getErrorMessage(error))
        if (!data) throw new Error('No response data from create API key')
        return data
      })
      apiKey = created.api_key
      newline()
      success(`Created API key "${created.name}" (${created.key_prefix}...)`)
    } else {
      apiKey = await requireAuth()
    }
  }

  const url = buildMcpUrl(apiUrl, groups, write)

  newline()
  header(`Configure ${adapter.label}`)
  keyValue('Target', await adapter.describeTarget())
  keyValue('URL', url)
  keyValue('Groups', groups.length === TOOL_GROUPS.length ? 'all' : groups.join(', '))
  keyValue('Write tools', write ? 'enabled' : 'disabled (read-only)')
  newline()

  if (!options.yes) {
    const proceed = await promptConfirm({ message: 'Apply this configuration?', default: true })
    if (!proceed) {
      info('Cancelled -- no changes made.')
      return
    }
  }

  const result = await adapter.addServer({ url, apiKey })
  if (!result.success) {
    warning(`Failed to configure ${adapter.label}: ${result.reason}`)
    return
  }

  if (result.alreadyInstalled) {
    info(`${adapter.label} is already configured with this exact setup.`)
  } else {
    success(`${adapter.label} configured.`)
  }
  info(`Restart ${adapter.label} to pick up the change.`)
}

async function removeAction(clientArg: string | undefined): Promise<void> {
  const clientId =
    clientArg ||
    (await promptSelect({
      message: 'Which AI client do you want to remove the Temps MCP server from?',
      choices: CLIENT_ADAPTERS.map((c) => ({ name: c.label, value: c.id })),
    }))

  const adapter = getClientAdapter(clientId)
  if (!adapter) {
    warning(`Unknown client "${clientId}". Available: ${listClientIds().join(', ')}`)
    return
  }

  const result = await adapter.removeServer()
  if (!result.success) {
    warning(`Failed to remove from ${adapter.label}: ${result.reason}`)
    return
  }

  if (result.alreadyInstalled) {
    info(`${adapter.label} had no Temps MCP server configured.`)
  } else {
    success(`Removed the Temps MCP server from ${adapter.label}.`)
  }
}

interface StatusRow {
  label: string
  detected: string
  installed: string
}

async function statusAction(): Promise<void> {
  header('Temps MCP client status')
  newline()

  const rows: StatusRow[] = await Promise.all(
    CLIENT_ADAPTERS.map(async (adapter) => {
      const [detected, installed] = await Promise.all([adapter.isClientSupported(), adapter.isServerInstalled()])
      return {
        label: adapter.label,
        detected: detected ? `${icons.success} detected` : `${icons.bullet} not detected`,
        installed: installed ? `${icons.success} configured` : `${icons.bullet} not configured`,
      }
    }),
  )

  const columns: TableColumn<StatusRow>[] = [
    { header: 'Client', key: 'label' },
    { header: 'On this machine', key: 'detected' },
    { header: 'Temps MCP server', key: 'installed' },
  ]

  printTable(rows, columns)
}
