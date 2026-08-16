import { expect, expectAppMounted, test } from '../fixtures'

test.describe('command palette', () => {
  test.beforeEach(async ({ page }) => {
    await page.route('**/api/user/me', async (route) => {
      await route.fulfill({
        contentType: 'application/json',
        json: {
          avatar_url: '',
          email: 'verify@temps.sh',
          id: 999,
          mfa_enabled: false,
          name: 'Verification User',
          role: 'admin',
          username: 'verify',
        },
      })
    })

    await page.route('**/api/projects?*', async (route) => {
      await route.fulfill({
        contentType: 'application/json',
        json: {
          page: 1,
          per_page: 100,
          projects: [
            {
              id: 999,
              name: 'Monitoring App',
              slug: 'monitoring-app',
            },
          ],
          total: 1,
        },
      })
    })

    await page.goto('/settings')
    await expectAppMounted(page)
    await page.getByRole('button', { name: /Find/ }).click()
    await expect(page.getByRole('combobox')).toBeVisible()
  })

  test('renders a left icon for actions', async ({ page }) => {
    await page.getByRole('combobox').fill('toggle')

    const missingLeftIcons = await page
      .locator('[cmdk-item]')
      .evaluateAll((items) =>
        items
          .filter((item) => {
            const firstChild = item.firstElementChild
            if (!firstChild) return true
            return !(
              firstChild.tagName.toLowerCase() === 'svg' ||
              (firstChild.tagName.toLowerCase() === 'span' &&
                firstChild.classList.contains('size-6'))
            )
          })
          .map((item) => item.textContent?.trim())
      )
    expect(missingLeftIcons).toEqual([])

    const themeAction = page.locator(
      '[cmdk-item][data-value="action:toggle-theme"]'
    )
    await expect(themeAction).toBeVisible()
    await expect(themeAction.locator(':scope > svg')).toHaveCount(1)
  })

  // Typing collapses the palette into ONE relevance-ranked list headed
  // "Results" (the fixed per-section groups only render on an empty input) --
  // so ordering, not grouping, is what carries the priority guarantee now.
  // The section a hit came from rides along as a right-aligned label.
  test('prioritizes project matches over common pages', async ({ page }) => {
    await page.getByRole('combobox').fill('monitor')

    const headings = page.locator('[cmdk-group-heading]')
    await expect(headings.first()).toHaveText('Results')

    const items = page.locator('[cmdk-item]')
    const project = page.locator('[cmdk-item][data-value="project:999"]')
    const navigation = page.locator(
      '[cmdk-item][data-value="/monitoring/alerts"]'
    )

    await expect(project).toBeVisible()
    await expect(navigation).toBeVisible()

    // The actual guarantee: the project outranks the similarly-named common
    // page. Both earn an identical title boost ("monitor" prefixes both
    // "monitoring-app" and "Monitoring"), so the ordering rests on the Fuse
    // score alone -- 0.5990 vs 0.5958 at the time of writing. That margin is
    // thin by design of the scoring, which is exactly why it is pinned here:
    // a scoring or keyword change that flips it should fail this test.
    const values = await items.evaluateAll((elements) =>
      elements.map((element) => element.getAttribute('data-value'))
    )
    const projectIndex = values.indexOf('project:999')
    const navigationIndex = values.indexOf('/monitoring/alerts')
    expect(projectIndex).toBeGreaterThanOrEqual(0)
    expect(navigationIndex).toBeGreaterThanOrEqual(0)
    expect(projectIndex).toBeLessThan(navigationIndex)

    // Each hit still says which section it came from, since the grouping no
    // longer does.
    await expect(project.locator(':scope > span').last()).toHaveText('Project')
    await expect(navigation.locator(':scope > span').last()).toHaveText(
      'Navigation'
    )

    // Icons survive the flattening: projects render an avatar, pages an svg.
    await expect(project.locator(':scope > span').first()).toBeVisible()
    await expect(navigation.locator(':scope > svg')).toHaveCount(1)
  })
})
