type SidebarTooltipState = {
  isMinimal: boolean
  isMobile: boolean
  state: 'expanded' | 'collapsed'
}

export function isSidebarMenuTooltipVisible({
  isMinimal,
  isMobile,
  state,
}: SidebarTooltipState) {
  return !isMobile && (isMinimal || state === 'collapsed')
}
