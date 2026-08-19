import { requireAuth } from '../../config/store.js'
import { setupClient, client, getErrorMessage } from '../../lib/api-client.js'
import type { ProblemDetails } from '../../api/types.gen.js'
import { withSpinner } from '../../ui/spinner.js'
import { printTable, type TableColumn } from '../../ui/table.js'
import { newline, header, icons, json, colors, info, warning } from '../../ui/output.js'

// ============================================================================
// Hand-written request/response shapes
// ============================================================================
//
// `GET /x/plugins/available` and `GET /x/plugins/{name}/status` are core
// routes owned by crates/temps-external-plugins/src/handler.rs, but the
// committed openapi.json has not yet been regenerated to include them (only
// the pre-existing `GET /x/plugins` and `POST /x/plugins/reload` routes are
// currently in the generated client — see src/api/sdk.gen.ts). Until
// `bun run spec:update` picks these up, hand-write local interfaces mirroring
// the handler's serde structs and call the shared `client` object's generic
// methods directly (same fallback pattern used for plugin-crate-owned routes
// in src/commands/otel-forward/index.ts) — keep these in sync by hand if the
// server-side shape changes, and delete them once the generated client gains
// the real types.

export interface PlatformAsset {
  url: string
  sha256: string
}

export interface PluginRegistryManifest {
  name: string
  version: string
  platforms: Record<string, PlatformAsset>
}

export interface PluginAvailabilityResponse {
  installed: boolean
  manifest?: PluginRegistryManifest | null
  reason?: string | null
}

export interface PluginStatusResponse {
  configured: boolean
  reason?: string | null
  setup_path?: string | null
}

function throwPluginsError(response: Response | undefined, error: unknown): never {
  throw new Error(getErrorMessage(error) || (response ? `Request failed with status ${response.status}` : 'Unknown error'))
}

interface PluginRow {
  name: string
  version: string
  installed: boolean
}

export async function listPluginsAction(options: { json?: boolean }): Promise<void> {
  await requireAuth()
  await setupClient()

  const availability = await withSpinner('Checking available plugins...', async () => {
    const { data, error, response } = await client.get<PluginAvailabilityResponse, ProblemDetails>({
      url: '/x/plugins/available',
    })
    if (error || !data) {
      throwPluginsError(response, error)
    }
    return data
  })

  if (options.json) {
    json(availability)
    return
  }

  newline()
  header(`${icons.info} Available Plugins`)

  if (!availability.manifest) {
    warning(availability.reason ?? 'Plugin registry manifest could not be fetched')
    newline()
    return
  }

  const rows: PluginRow[] = [
    {
      name: availability.manifest.name,
      version: availability.manifest.version,
      installed: availability.installed,
    },
  ]

  const columns: TableColumn<PluginRow>[] = [
    { header: 'Name', key: 'name', color: (v) => colors.bold(v) },
    { header: 'Version', key: 'version' },
    {
      header: 'Installed',
      accessor: (r) => (r.installed ? 'yes' : 'no'),
      color: (v) => (v === 'yes' ? colors.success(v) : colors.muted(v)),
    },
  ]

  printTable(rows, columns, { style: 'minimal' })

  if (!availability.installed) {
    info(`Run: temps plugins install ${availability.manifest.name}`)
  }
  newline()
}
