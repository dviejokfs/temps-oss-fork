import { McpServerCard } from '@/components/settings/McpServerCard'
import { useBreadcrumbs } from '@/contexts/BreadcrumbContext'
import { usePageTitle } from '@/hooks/usePageTitle'
import { useEffect } from 'react'

export function McpServerPage() {
  const { setBreadcrumbs } = useBreadcrumbs()

  useEffect(() => {
    setBreadcrumbs([{ label: 'Settings', href: '/settings' }, { label: 'MCP Server' }])
  }, [setBreadcrumbs])

  usePageTitle('MCP Server')

  return (
    <div className="space-y-6">
      <McpServerCard />
    </div>
  )
}
