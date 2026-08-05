import type { Command } from 'commander'
import { execSync } from 'node:child_process'
import { credentials } from '../../config/store.js'
import {
  info,
  newline,
  colors,
  box,
  icons,
  header,
  keyValue,
  error as errorOutput,
} from '../../ui/output.js'
import { startSpinner, succeedSpinner, failSpinner, updateSpinner, withSpinner } from '../../ui/spinner.js'
import { getCloudUrl, cloudFetch, isCloudAuthenticated } from '../../lib/cloud-client.js'
import { registerCloudVpsCommands } from './vps.js'
import { registerCloudBillingCommands } from './billing.js'
import { requireAuth } from '../../config/store.js'
import { client, getErrorMessage, setupClient } from '../../lib/api-client.js'
import {
  disconnectCloud as disconnectInstance,
  enrollCloud as enrollInstance,
  getCloudStatus as getInstanceCloudStatus,
} from '../../api/sdk.gen.js'

interface DeviceCodeResponse {
  device_code: string
  user_code: string
  verification_uri: string
  verification_uri_complete: string
  expires_in: number
  interval: number
}

interface DeviceTokenResponse {
  access_token: string
  token_type: string
}

interface DeviceTokenError {
  error: 'authorization_pending' | 'access_denied' | 'expired_token' | string
}

interface UserProfile {
  id: number
  email?: string
  username: string
  name: string
  plan?: string
}

function openBrowser(url: string): void {
  try {
    if (process.platform === 'darwin') {
      execSync(`open "${url}"`)
    } else if (process.platform === 'win32') {
      execSync(`start "" "${url}"`)
    } else {
      execSync(`xdg-open "${url}"`)
    }
  } catch {
    // Browser open failed, user will use the manual URL
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms))
}

async function cloudLogin(): Promise<void> {
  const cloudUrl = getCloudUrl()

  newline()
  info(`Connecting to Temps Cloud at ${colors.primary(cloudUrl)}`)
  newline()

  // Step 1: Request device code
  startSpinner('Requesting authorization...')

  let deviceCode: DeviceCodeResponse
  try {
    const response = await fetch(`${cloudUrl}/api/auth/device`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ client_name: 'Temps CLI' }),
    })

    if (!response.ok) {
      throw new Error(`Failed to start device authorization (${response.status})`)
    }

    deviceCode = await response.json() as DeviceCodeResponse
    succeedSpinner('Authorization started')
  } catch (err) {
    failSpinner('Failed to connect to Temps Cloud')
    throw err
  }

  // Step 2: Show user code and open browser
  newline()
  box(
    `Your code: ${colors.bold(deviceCode.user_code)}\n\n` +
    `Opening ${colors.primary(deviceCode.verification_uri_complete)}\n` +
    `in your browser...`,
    `${icons.globe} Authorize Temps CLI`
  )
  newline()

  openBrowser(deviceCode.verification_uri_complete)

  info(`If the browser didn't open, visit: ${colors.primary(deviceCode.verification_uri)}`)
  info(`and enter code: ${colors.bold(deviceCode.user_code)}`)
  newline()

  // Step 3: Poll for token
  startSpinner('Waiting for authorization...')

  const interval = (deviceCode.interval || 5) * 1000
  const deadline = Date.now() + deviceCode.expires_in * 1000
  let accessToken: string | null = null

  while (Date.now() < deadline) {
    await sleep(interval)

    try {
      const response = await fetch(`${cloudUrl}/api/auth/device/token`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ device_code: deviceCode.device_code }),
      })

      if (response.ok) {
        const data = await response.json() as DeviceTokenResponse
        accessToken = data.access_token
        break
      }

      const body = await response.json() as DeviceTokenError

      if (body.error === 'authorization_pending') {
        updateSpinner('Waiting for authorization...')
        continue
      }

      if (body.error === 'access_denied') {
        failSpinner('Authorization denied')
        errorOutput('You denied the authorization request.')
        return
      }

      if (body.error === 'expired_token') {
        failSpinner('Authorization expired')
        errorOutput('The authorization code expired. Run "temps cloud login" again.')
        return
      }

      // Unknown error, keep polling
      continue
    } catch {
      // Network error, keep polling
      continue
    }
  }

  if (!accessToken) {
    failSpinner('Authorization timed out')
    errorOutput('The authorization code expired. Run "temps cloud login" again.')
    return
  }

  succeedSpinner('Authorized')

  // Step 4: Fetch user profile
  startSpinner('Fetching account info...')

  let profile: UserProfile
  try {
    const response = await fetch(`${cloudUrl}/api/user`, {
      headers: { 'Authorization': `Bearer ${accessToken}` },
    })

    if (!response.ok) {
      throw new Error(`Failed to fetch user profile (${response.status})`)
    }

    profile = await response.json() as UserProfile
    succeedSpinner(`Logged in as ${profile.email ?? profile.username}`)
  } catch (err) {
    failSpinner('Failed to fetch account')
    throw err
  }

  // Step 5: Store cloud API key (separate from self-hosted apiKey)
  await credentials.set('cloudApiKey', accessToken)

  newline()
  box(
    `Logged in as ${colors.bold(profile.email ?? profile.username)}\n` +
    `API: ${colors.muted(cloudUrl)}`,
    `${icons.sparkles} Welcome to Temps Cloud!`
  )
}

async function cloudLogout(): Promise<void> {
  const authenticated = await isCloudAuthenticated()

  if (!authenticated) {
    info('Not logged in to Temps Cloud.')
    return
  }

  await credentials.set('cloudApiKey', undefined)
  newline()
  info('Logged out of Temps Cloud.')
}

async function cloudWhoami(): Promise<void> {
  const profile = await cloudFetch<UserProfile>('/api/user')

  newline()
  header(`${icons.globe} Temps Cloud Account`)
  keyValue('ID', profile.id)
  keyValue('Name', profile.name)
  keyValue('Username', profile.username)
  keyValue('Email', profile.email ?? 'not set')
  if (profile.plan) {
    keyValue('Plan', profile.plan)
  }
  newline()
}

async function instanceStatus(options: { json?: boolean }): Promise<void> {
  await requireAuth()
  await setupClient()
  const result = await withSpinner('Reading Temps Cloud link...', async () => {
    const { data, error } = await getInstanceCloudStatus({ client })
    if (error) throw new Error(getErrorMessage(error))
    return data
  })
  if (!result) return
  if (options.json) {
    process.stdout.write(`${JSON.stringify(result, null, 2)}\n`)
    return
  }
  newline()
  header(`${icons.globe} Instance Cloud Link`)
  keyValue('Status', result.status.replaceAll('_', ' '))
  keyValue('Health', result.health.replaceAll('_', ' '))
  keyValue('Instance', result.instance_id ?? 'not enrolled')
  keyValue('Buffered spans', result.spooled_spans)
  info(result.status_message)
  if (result.health !== 'healthy') info(result.health_message)
  newline()
}

async function connectInstance(options: { code: string }): Promise<void> {
  await requireAuth()
  await setupClient()
  const result = await withSpinner('Connecting this instance...', async () => {
    const { data, error } = await enrollInstance({
      client,
      body: { enrollment_code: options.code },
    })
    if (error) throw new Error(getErrorMessage(error))
    return data
  })
  if (result) info(`Connected. ${result.status_message}`)
}

async function disconnectCurrentInstance(): Promise<void> {
  await requireAuth()
  await setupClient()
  await withSpinner('Disconnecting this instance...', async () => {
    const { error } = await disconnectInstance({ client })
    if (error) throw new Error(getErrorMessage(error))
  })
  info('Instance disconnected from Temps Cloud')
}

export function registerCloudCommands(program: Command): void {
  const cloud = program
    .command('cloud')
    .description('Temps Cloud')

  cloud
    .command('login')
    .description('Login to Temps Cloud')
    .action(cloudLogin)

  cloud
    .command('logout')
    .description('Logout from Temps Cloud')
    .action(cloudLogout)

  cloud
    .command('whoami')
    .description('Show current Temps Cloud account')
    .action(cloudWhoami)

  cloud
    .command('status')
    .description('Show this self-hosted instance\'s Temps Cloud link')
    .option('--json', 'Output JSON')
    .action(instanceStatus)

  cloud
    .command('connect')
    .description('Connect this self-hosted instance using an enrollment code')
    .requiredOption('--code <code>', 'Single-use enrollment code from Temps Cloud')
    .action(connectInstance)

  cloud
    .command('disconnect')
    .description('Disconnect this self-hosted instance from Temps Cloud')
    .action(disconnectCurrentInstance)

  registerCloudVpsCommands(cloud)
  registerCloudBillingCommands(cloud)
}
