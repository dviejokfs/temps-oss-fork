import type { Command } from 'commander'
import { requireAuth } from '../../config/store.js'
import { setupClient, client, getErrorMessage } from '../../lib/api-client.js'
import {
  getPlatformInfo,
  getAccessInfo,
  getPrivateIp,
  getPublicIp,
  getUpdateCapability,
  getUpdateStatus,
  startUpdate,
} from '../../api/sdk.gen.js'
import { withSpinner } from '../../ui/spinner.js'
import { promptConfirm } from '../../ui/prompts.js'
import { newline, header, icons, json, colors, info, keyValue, success, warning, error, formatDate } from '../../ui/output.js'

export function registerPlatformCommands(program: Command): void {
  const platform = program
    .command('platform')
    .alias('plat')
    .description('View platform and server information')

  platform
    .command('info')
    .description('Get platform information')
    .option('--json', 'Output in JSON format')
    .action(platformInfoAction)

  platform
    .command('access')
    .description('Get access and networking information')
    .option('--json', 'Output in JSON format')
    .action(accessInfoAction)

  platform
    .command('private-ip')
    .description('Get the server private IP address')
    .action(privateIpAction)

  platform
    .command('public-ip')
    .description('Get the server public IP address')
    .action(publicIpAction)

  const update = platform
    .command('update')
    .description('Check for and apply temps releases on the server')

  update
    .command('status')
    .description('Show the available release and whether it can be applied from here')
    .option('--json', 'Output in JSON format')
    .action(updateStatusAction)

  update
    .command('apply')
    .description('Install a release on the server and restart it')
    .option('--version <version>', 'Release tag to install (default: newest on this channel)')
    .option('-y, --yes', 'Skip the confirmation prompt')
    .option('--json', 'Output in JSON format')
    .action(updateApplyAction)
}

async function platformInfoAction(options: { json?: boolean }): Promise<void> {
  await requireAuth()
  await setupClient()

  const platformInfo = await withSpinner('Fetching platform info...', async () => {
    const { data, error } = await getPlatformInfo({ client })
    if (error) {
      throw new Error(getErrorMessage(error))
    }
    return data
  })

  if (!platformInfo) {
    info('Platform information not available')
    return
  }

  if (options.json) {
    json(platformInfo)
    return
  }

  newline()
  header(`${icons.globe} Platform Information`)
  keyValue('OS Type', platformInfo.os_type)
  keyValue('Architecture', platformInfo.architecture)
  keyValue('Platforms', platformInfo.platforms.join(', ') || colors.muted('none'))
  newline()
}

async function accessInfoAction(options: { json?: boolean }): Promise<void> {
  await requireAuth()
  await setupClient()

  const accessData = await withSpinner('Fetching access info...', async () => {
    const { data, error } = await getAccessInfo({ client })
    if (error) {
      throw new Error(getErrorMessage(error))
    }
    return data
  })

  if (!accessData) {
    info('Access information not available')
    return
  }

  if (options.json) {
    json(accessData)
    return
  }

  newline()
  header(`${icons.globe} Access Information`)
  keyValue('Access Mode', accessData.access_mode)
  keyValue('Can Create Domains', accessData.can_create_domains ? colors.success('Yes') : colors.muted('No'))
  if (accessData.domain_creation_error) {
    keyValue('Domain Error', colors.warning(accessData.domain_creation_error))
  }
  keyValue('Public IP', accessData.public_ip || colors.muted('not available'))
  keyValue('Private IP', accessData.private_ip || colors.muted('not available'))
  newline()
}

async function privateIpAction(): Promise<void> {
  await requireAuth()
  await setupClient()

  const result = await withSpinner('Fetching private IP...', async () => {
    const { data, error } = await getPrivateIp({ client })
    if (error) {
      throw new Error(getErrorMessage(error))
    }
    return data
  })

  if (result && typeof result === 'object' && 'ip' in result) {
    success(String((result as { ip: string }).ip))
  } else if (typeof result === 'string') {
    success(result)
  } else {
    json(result)
  }
}

async function publicIpAction(): Promise<void> {
  await requireAuth()
  await setupClient()

  const result = await withSpinner('Fetching public IP...', async () => {
    const { data, error } = await getPublicIp({ client })
    if (error) {
      throw new Error(getErrorMessage(error))
    }
    return data
  })

  if (result && typeof result === 'object' && 'ip' in result) {
    success(String((result as { ip: string }).ip))
  } else if (typeof result === 'string') {
    success(result)
  } else {
    json(result)
  }
}

/**
 * Report both halves of the picture: whether a newer release exists, and
 * whether this install can apply it without someone SSH-ing to the host.
 */
async function updateStatusAction(options: { json?: boolean }): Promise<void> {
  await requireAuth()
  await setupClient()

  const { status, capability } = await withSpinner('Checking for updates...', async () => {
    const [statusRes, capabilityRes] = await Promise.all([
      getUpdateStatus({ client }),
      getUpdateCapability({ client }),
    ])
    if (statusRes.error) {
      throw new Error(getErrorMessage(statusRes.error))
    }
    if (capabilityRes.error) {
      throw new Error(getErrorMessage(capabilityRes.error))
    }
    return { status: statusRes.data, capability: capabilityRes.data }
  })

  if (options.json) {
    json({ status, capability })
    return
  }

  newline()
  header(`${icons.globe} Platform Updates`)

  if (status?.update_available) {
    keyValue('Current', status.current_version ?? colors.muted('unknown'))
    keyValue('Available', colors.success(status.latest_version ?? 'unknown'))
    keyValue('Channel', status.channel ?? colors.muted('unknown'))
    if (status.release_url) {
      keyValue('Release notes', status.release_url)
    }
  } else {
    keyValue('Status', 'Up to date (or no check has completed yet)')
    keyValue('Current', status?.current_version ?? colors.muted('unknown'))
  }

  newline()
  keyValue('Supervisor', capability?.supervisor ?? colors.muted('unknown'))
  keyValue('Binary', capability?.binary_path || colors.muted('unknown'))

  if (capability?.can_apply && capability.allowed) {
    keyValue('Apply from here', colors.success('Yes'))
    info(`Run ${colors.primary('temps platform update apply')} to install and restart.`)
  } else {
    keyValue('Apply from here', colors.warning('No'))
    if (!capability?.allowed) {
      warning('Your credentials lack the platform:update permission.')
    }
    if (capability?.reason) {
      warning(capability.reason)
    }
    // Never leave the operator without a way forward.
    info(`Upgrade manually on the host: ${colors.primary(capability?.manual_command ?? 'temps upgrade')}`)
  }

  if (capability?.caveat) {
    warning(capability.caveat)
  }

  // Surface how the previous attempt ended — especially a failure, which the
  // person running this may never have seen if it happened in the console.
  const attempt = capability?.last_attempt
  if (attempt) {
    newline()
    header(`${icons.bullet} Last update attempt`)
    keyValue('Result', attempt.status === 'succeeded'
      ? colors.success('succeeded')
      : attempt.status === 'failed'
        ? colors.error('failed')
        : 'in progress')
    keyValue('From', attempt.from_version)
    keyValue('To', attempt.to_version ?? colors.muted('not resolved'))
    keyValue('Started', formatDate(attempt.started_at))
    if (attempt.error) {
      warning(attempt.error)
    }
    if (attempt.previous_binary_path) {
      keyValue('Previous binary', attempt.previous_binary_path)
    }
  }
  newline()
}

/**
 * Apply a release. The server restarts itself, so this deliberately does NOT
 * wait for a response body beyond the acceptance — it polls afterwards and
 * reports the recorded outcome once the server answers again.
 */
async function updateApplyAction(options: {
  version?: string
  yes?: boolean
  json?: boolean
}): Promise<void> {
  await requireAuth()
  await setupClient()

  const capability = await withSpinner('Checking update capability...', async () => {
    const { data, error: err } = await getUpdateCapability({ client })
    if (err) {
      throw new Error(getErrorMessage(err))
    }
    return data
  })

  // Fail before prompting when the server has already told us it cannot.
  if (!capability?.can_apply || !capability.allowed) {
    error('This server cannot apply updates right now.')
    if (!capability?.allowed) {
      warning('Your credentials lack the platform:update permission.')
    }
    if (capability?.reason) {
      warning(capability.reason)
    }
    info(`Upgrade manually on the host: ${colors.primary(capability?.manual_command ?? 'temps upgrade')}`)
    process.exitCode = 1
    return
  }

  if (capability.caveat) {
    warning(capability.caveat)
  }

  if (!options.yes) {
    const confirmed = await promptConfirm({
      message: `Install ${options.version ?? 'the latest release'} and restart the server? It will be briefly unavailable.`,
      default: false,
    })
    if (!confirmed) {
      info('Cancelled.')
      return
    }
  }

  const started = await withSpinner('Starting update...', async () => {
    const { data, error: err } = await startUpdate({
      client,
      body: options.version ? { version: options.version } : {},
    })
    if (err) {
      throw new Error(getErrorMessage(err))
    }
    return data
  })

  if (options.json) {
    json(started)
    return
  }

  success(`Update started from ${started?.current_version ?? 'the running version'}.`)
  info('The server is downloading, verifying and installing, then restarting.')

  const outcome = await withSpinner('Waiting for the server to come back...', async () =>
    waitForUpdateOutcome(
      capability.last_attempt?.started_at ?? null,
      (started?.estimated_restart_secs ?? 45) + 60
    )
  )

  newline()
  if (!outcome) {
    warning('The server did not report an outcome in time.')
    info('Check the service on the host (systemctl status temps / journalctl -u temps).')
    process.exitCode = 1
    return
  }
  if (outcome.status === 'succeeded') {
    success(`temps is now running ${outcome.to_version} (was ${outcome.from_version}).`)
    return
  }
  error(outcome.error ?? 'The update did not complete.')
  info(`The server is still running ${outcome.from_version}.`)
  if (outcome.previous_binary_path) {
    info(`Previous binary kept at ${outcome.previous_binary_path}`)
  }
  process.exitCode = 1
}

/**
 * Poll until the attempt we just started resolves.
 *
 * Connection errors are expected and ignored — the server is restarting, which
 * is the whole point. `baselineStartedAt` distinguishes our attempt from a
 * result left over from a previous one.
 */
async function waitForUpdateOutcome(
  baselineStartedAt: string | null,
  timeoutSecs: number
): Promise<
  | {
      status: string
      from_version: string
      to_version?: string | null
      error?: string | null
      previous_binary_path?: string | null
    }
  | null
> {
  const deadline = Date.now() + timeoutSecs * 1000
  while (Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 2000))
    try {
      const { data } = await getUpdateCapability({ client })
      const attempt = data?.last_attempt
      if (
        attempt &&
        attempt.status !== 'pending' &&
        attempt.started_at !== baselineStartedAt
      ) {
        return attempt
      }
    } catch {
      // Server is down mid-restart; keep waiting.
    }
  }
  return null
}
