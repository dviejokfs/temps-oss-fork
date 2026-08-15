import {
  Activity,
  AlarmClock,
  Bot,
  Clock,
  CreditCard,
  Eye,
  FileLock2,
  FileText,
  Filter,
  Flag,
  KeyRound,
  LineChart,
  Play,
  Radio,
  Rss,
  Server,
  Shield,
  SlidersHorizontal,
  Users,
  Wand2,
  Webhook,
  Workflow,
  Zap,
  type LucideIcon,
} from 'lucide-react'

export interface ProjectToolItem {
  title: string
  url: string
  icon: LucideIcon
}

export interface ProjectToolGroup {
  label: string
  description: string
  items: ProjectToolItem[]
}

export const projectToolGroups: ProjectToolGroup[] = [
  {
    label: 'Analytics',
    description: 'Understand traffic, content, conversion, and revenue.',
    items: [
      { title: 'Visitors', url: 'analytics/visitors', icon: Users },
      { title: 'Pages', url: 'analytics/pages', icon: FileText },
      { title: 'AI Agents', url: 'analytics/ai-agents', icon: Bot },
      { title: 'Funnels', url: 'analytics/funnels', icon: Filter },
      { title: 'Session Replays', url: 'analytics/replays', icon: Play },
      { title: 'API Traffic', url: 'analytics/api-traffic', icon: Server },
      { title: 'Speed', url: 'speed', icon: Zap },
      { title: 'Revenue', url: 'revenue', icon: CreditCard },
    ],
  },
  {
    label: 'Observe',
    description: 'Investigate runtime behavior, health, and incoming requests.',
    items: [
      { title: 'Activity', url: 'observe', icon: Eye },
      { title: 'AI Traces', url: 'ai-gateway?tab=activity', icon: Bot },
      { title: 'Metrics', url: 'metrics', icon: LineChart },
      { title: 'Telemetry Logs', url: 'telemetry-logs', icon: Radio },
      { title: 'Uptime', url: 'monitors', icon: Activity },
      { title: 'Request Logs', url: 'request-logs', icon: Rss },
      { title: 'AI Crawlers', url: 'ai-crawlers', icon: Bot },
    ],
  },
  {
    label: 'Configure',
    description: 'Connect runtime features and automate project workflows.',
    items: [
      {
        title: 'Environment Variables',
        url: 'environment-variables',
        icon: KeyRound,
      },
      { title: 'Feature Flags', url: 'flags', icon: Flag },
      { title: 'AI Workflows', url: 'agents', icon: Workflow },
    ],
  },
  {
    label: 'Project settings',
    description: 'Control secrets, access, schedules, and integrations.',
    items: [
      { title: 'General', url: 'settings/general', icon: SlidersHorizontal },
      { title: 'Secrets', url: 'settings/secrets', icon: FileLock2 },
      { title: 'Security', url: 'settings/security', icon: Shield },
      { title: 'Access', url: 'settings/access', icon: Users },
      { title: 'Cron Jobs', url: 'settings/cron-jobs', icon: Clock },
      { title: 'Webhooks', url: 'settings/webhooks', icon: Webhook },
      { title: 'Skills', url: 'settings/skills', icon: Wand2 },
      { title: 'MCP Servers', url: 'settings/mcp-servers', icon: Server },
      { title: 'Alert Rules', url: 'errors/alert-rules', icon: AlarmClock },
    ],
  },
]

export const projectToolShortcuts: Array<
  ProjectToolItem & { description: string }
> = [
  {
    title: 'Understand visitors',
    description: 'Journeys, sessions, and audience details',
    url: 'analytics/visitors',
    icon: Users,
  },
  {
    title: 'Inspect metrics',
    description: 'Explore resource and application signals',
    url: 'metrics',
    icon: LineChart,
  },
  {
    title: 'Check availability',
    description: 'Uptime monitors and incident history',
    url: 'monitors',
    icon: Activity,
  },
  {
    title: 'Manage runtime variables',
    description: 'Environment values and protected secrets',
    url: 'environment-variables',
    icon: KeyRound,
  },
]

export function flattenProjectTools(
  groups: ProjectToolGroup[]
): ProjectToolItem[] {
  return groups.flatMap((group) => group.items)
}
