import { describe, expect, test } from 'bun:test'

import { isSidebarMenuTooltipVisible } from './sidebar-tooltip'

describe('isSidebarMenuTooltipVisible', () => {
  test('shows menu titles in the compact desktop sidebar', () => {
    expect(
      isSidebarMenuTooltipVisible({
        isMinimal: true,
        isMobile: false,
        state: 'expanded',
      })
    ).toBe(true)
  })

  test('preserves tooltips for the legacy collapsed desktop state', () => {
    expect(
      isSidebarMenuTooltipVisible({
        isMinimal: false,
        isMobile: false,
        state: 'collapsed',
      })
    ).toBe(true)
  })

  test('hides redundant tooltips in expanded and mobile sidebars', () => {
    expect(
      isSidebarMenuTooltipVisible({
        isMinimal: false,
        isMobile: false,
        state: 'expanded',
      })
    ).toBe(false)
    expect(
      isSidebarMenuTooltipVisible({
        isMinimal: true,
        isMobile: true,
        state: 'expanded',
      })
    ).toBe(false)
  })
})
