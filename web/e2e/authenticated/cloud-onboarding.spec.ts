import type { Page } from '@playwright/test'
import { expect, expectAppMounted, test } from '../fixtures'

const cloudStatus = (linked: boolean) => ({
  account_email: linked ? 'owner@example.com' : null,
  backend_url: 'http://localhost:19200',
  health: linked ? 'healthy' : 'disconnected',
  health_message: linked ? 'Signals are reaching Temps Cloud' : 'Not linked',
  instance_id: linked ? 'instance-e2e-1234' : null,
  spooled_spans: 0,
  status: linked ? 'linked' : 'disconnected',
  status_message: linked
    ? 'This instance is reporting to Temps Cloud'
    : 'Connect this instance to begin reporting',
})

const routeCloudLifecycle = async (page: Page) => {
  let linked = false
  const enrollmentCodes: string[] = []

  await page.route('**/cloud/capability', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify({
        configured: true,
        reason: null,
        setup_path: '/settings/cloud',
      }),
    })
  })
  await page.route('**/cloud/status', async (route) => {
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(cloudStatus(linked)),
    })
  })
  await page.route('**/cloud/enroll', async (route) => {
    enrollmentCodes.push(route.request().postDataJSON().enrollment_code)
    linked = true
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(cloudStatus(true)),
    })
  })
  await page.route('**/cloud', async (route) => {
    if (route.request().method() !== 'DELETE') {
      await route.fallback()
      return
    }
    linked = false
    await route.fulfill({
      status: 200,
      contentType: 'application/json',
      body: JSON.stringify(cloudStatus(false)),
    })
  })

  return { enrollmentCodes }
}

test.describe('Temps Cloud activation onboarding', () => {
  test('connects and disconnects an instance from the two-step setup', async ({
    page,
    consoleErrors,
  }) => {
    const cloud = await routeCloudLifecycle(page)
    await page.goto('/settings/cloud')
    await expectAppMounted(page)

    await expect(
      page.getByRole('heading', { name: 'Connect this instance' })
    ).toBeVisible()
    await expect(
      page.getByRole('link', { name: 'Get a code' })
    ).toHaveAttribute('href', 'http://localhost:19200')
    await page.getByLabel('1. Paste enrollment code').fill('ABCD-EFGH')
    await page.getByRole('button', { name: '2. Connect' }).click()

    await expect(page.getByRole('heading', { name: 'Connected' })).toBeVisible()
    await expect(
      page.getByText('Cloud account: owner@example.com')
    ).toBeVisible()
    await expect(page.getByText('instance-e2')).toBeVisible()
    expect(cloud.enrollmentCodes).toEqual(['ABCD-EFGH'])

    await page.getByRole('button', { name: 'Disconnect' }).click()
    await expect(
      page.getByRole('heading', { name: 'Connect this instance' })
    ).toBeVisible()

    await page.getByLabel('1. Paste enrollment code').fill('WXYZ-IJKL')
    await page.getByRole('button', { name: '2. Connect' }).click()
    await expect(page.getByRole('heading', { name: 'Connected' })).toBeVisible()
    expect(cloud.enrollmentCodes).toEqual(['ABCD-EFGH', 'WXYZ-IJKL'])
    expect(consoleErrors).toEqual([])
  })
})
