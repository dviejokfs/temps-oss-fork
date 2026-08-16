import type { GettingStartedItem } from '@/hooks/useGettingStarted'

export function nextIncompleteGettingStartedItem(
  items: GettingStartedItem[]
): GettingStartedItem | undefined {
  return items.find((item) => !item.done)
}
