import type { Command } from 'commander'
import { requireAuth } from '../../config/store.js'
import { setupClient, client, getErrorMessage } from '../../lib/api-client.js'
import { listFacets, createFacet, deleteFacet } from '../../api/sdk.gen.js'
import type { FacetInfo } from '../../api/types.gen.js'
import { withSpinner } from '../../ui/spinner.js'
import { printTable, type TableColumn } from '../../ui/table.js'
import { promptConfirm } from '../../ui/prompts.js'
import { newline, header, icons, json, colors, success, info } from '../../ui/output.js'

export function registerFacetsCommands(program: Command): void {
  const facets = program
    .command('facets')
    .description(
      'Manage OTel span attribute facets — attribute keys promoted to a fast-filterable ' +
        'ClickHouse column (see ADR-039). Facets are platform-global, not per-project, since ' +
        'the underlying `spans` table is shared across every project.',
    )

  facets
    .command('list')
    .alias('ls')
    .description('List registered span attribute facets')
    .option('--json', 'Output in JSON format')
    .action(listFacetsAction)

  facets
    .command('create <attribute-key>')
    .description(
      'Register an attribute key as a facet, making it fast to filter on across all traces. ' +
        'Backfills existing spans that carry the attribute. Capped at 20 facets platform-wide.',
    )
    .option('--json', 'Output in JSON format')
    .action(createFacetAction)

  facets
    .command('remove <attribute-key>')
    .alias('rm')
    .description('Remove a registered facet, freeing its slot for reuse')
    .option('-f, --force', 'Skip confirmation')
    .option('-y, --yes', 'Skip confirmation prompts (alias for --force)')
    .action(removeFacetAction)
}

async function listFacetsAction(options: { json?: boolean }): Promise<void> {
  await requireAuth()
  await setupClient()

  const result = await withSpinner('Fetching span attribute facets...', async () => {
    const { data, error } = await listFacets({ client })
    if (error) throw new Error(getErrorMessage(error))
    if (!data) throw new Error('No response from server')
    return data
  })

  if (options.json) {
    json(result)
    return
  }

  newline()
  header(`${icons.info} Span Attribute Facets (${result.data.length}/20)`)

  if (result.data.length === 0) {
    info('No facets registered')
    info('Run: temps facets create <attribute-key>')
    newline()
    return
  }

  const columns: TableColumn<FacetInfo>[] = [
    { header: 'Attribute Key', key: 'attribute_key', color: (v) => colors.bold(v) },
    { header: 'Slot', accessor: (f) => `facet_attr_${f.slot}`, color: (v) => colors.muted(v) },
    { header: 'Created', key: 'created_at' },
  ]

  printTable(result.data, columns, { style: 'minimal' })
  newline()
}

async function createFacetAction(
  attributeKey: string,
  options: { json?: boolean },
): Promise<void> {
  await requireAuth()
  await setupClient()

  const facet = await withSpinner(`Registering facet "${attributeKey}"...`, async () => {
    const { data, error } = await createFacet({
      client,
      body: { attribute_key: attributeKey },
    })
    if (error) throw new Error(getErrorMessage(error))
    if (!data) throw new Error('No response from server')
    return data
  })

  if (options.json) {
    json(facet)
    return
  }

  success(`Facet "${facet.attribute_key}" registered in slot ${facet.slot}`)
  info('Existing spans carrying this attribute have been backfilled and are now fast-filterable.')
}

async function removeFacetAction(
  attributeKey: string,
  options: { force?: boolean; yes?: boolean },
): Promise<void> {
  await requireAuth()
  await setupClient()

  const skipConfirmation = options.force || options.yes

  if (!skipConfirmation) {
    const confirmed = await promptConfirm({
      message: `Remove facet "${attributeKey}"? Filtering on it will fall back to a full attribute scan.`,
      default: false,
    })
    if (!confirmed) {
      info('Cancelled')
      return
    }
  }

  await withSpinner(`Removing facet "${attributeKey}"...`, async () => {
    const { error } = await deleteFacet({
      client,
      path: { key: attributeKey },
    })
    if (error) throw new Error(getErrorMessage(error))
  })

  success(`Facet "${attributeKey}" removed`)
}
