import type { ProjectResponse } from '@/api/client'
import { getLastDeploymentOptions } from '@/api/client/@tanstack/react-query.gen'
import { PresetIcon } from '@/components/presets/PresetIcon'
import { Avatar, AvatarFallback, AvatarImage } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import { KbdBadge } from '@/components/ui/kbd-badge'
import { Skeleton } from '@/components/ui/skeleton'
import { ReloadableImage } from '@/components/utils/ReloadableImage'
import { TimeAgo } from '@/components/utils/TimeAgo'
import type { ProjectDashboardAnalytics } from '@/hooks/useDashboardAnalytics'
import type { ProjectMonitorHealth } from '@/hooks/useDashboardHealth'
import GithubIcon from '@/icons/Github'
import GitlabIcon from '@/icons/Gitlab'
import { useQuery } from '@tanstack/react-query'
import { AlertCircle, GitFork, Network, PackageOpen, Users } from 'lucide-react'
import { Link } from 'react-router'
import {
  deploymentLabel,
  projectPresetLabel,
  projectRepository,
  type RepositoryInfo,
} from './project-card-data'

interface ProjectCardProps {
  project: ProjectResponse
  shortcutNumber?: number
  analytics?: ProjectDashboardAnalytics
  analyticsLoading?: boolean
  analyticsError?: boolean
  apiRequests?: number
  apiTrafficLoading?: boolean
  apiTrafficError?: boolean
  health?: ProjectMonitorHealth
}

function HealthStatusDot({ status }: { status: string }) {
  const colors: Record<string, string> = {
    operational: 'bg-emerald-500',
    degraded: 'bg-amber-500',
    down: 'bg-red-500',
    no_monitors: 'bg-zinc-400',
    unknown: 'bg-zinc-400',
  }
  const label =
    status === 'no_monitors'
      ? 'No monitors'
      : status.charAt(0).toUpperCase() + status.slice(1)

  return (
    <span
      className={`inline-block size-2 rounded-full ${colors[status] || colors.unknown}`}
      title={label}
      aria-label={label}
    />
  )
}

function RepositoryIcon({
  provider,
}: {
  provider: RepositoryInfo['provider']
}) {
  if (provider === 'github') return <GithubIcon className="size-4 shrink-0" />
  if (provider === 'gitlab') return <GitlabIcon className="size-4 shrink-0" />
  return <GitFork className="size-4 shrink-0 text-muted-foreground" />
}

function MetadataCell({
  label,
  children,
}: {
  label: string
  children: React.ReactNode
}) {
  return (
    <div className="min-w-0">
      <div className="mb-1.5 text-[10px] font-semibold uppercase tracking-wider text-muted-foreground">
        {label}
      </div>
      {children}
    </div>
  )
}

export function ProjectCard({
  project,
  shortcutNumber,
  analytics,
  analyticsLoading = false,
  analyticsError = false,
  apiRequests = 0,
  apiTrafficLoading = false,
  apiTrafficError = false,
  health,
}: ProjectCardProps) {
  const repository = projectRepository(project)
  const totalVisitors = analytics?.unique_visitors ?? 0

  const { data: lastDeployment } = useQuery({
    ...getLastDeploymentOptions({
      path: { id: project.id },
    }),
    enabled: !!project.id,
    refetchOnWindowFocus: true,
  })

  return (
    <Link
      to={`/projects/${project.slug}`}
      className="group grid gap-4 px-4 py-3.5 transition-colors hover:bg-muted/50 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring lg:grid-cols-[minmax(14rem,0.9fr)_minmax(28rem,1.7fr)_minmax(11rem,0.65fr)_2.5rem] lg:items-center"
    >
      <div className="flex min-w-0 items-center gap-3">
        {lastDeployment?.screenshot_location ? (
          <div className="size-9 shrink-0 overflow-hidden rounded-md border bg-muted/30">
            <ReloadableImage
              src={`/api/files${lastDeployment.screenshot_location.startsWith('/') ? lastDeployment.screenshot_location : `/${lastDeployment.screenshot_location}`}`}
              alt={`${project.slug} preview`}
              className="size-full object-cover object-top"
            />
          </div>
        ) : (
          <Avatar className="size-9 shrink-0 rounded-md">
            <AvatarImage src={`/api/projects/${project.id}/favicon`} />
            <AvatarFallback className="rounded-md">
              {project.name.charAt(0)}
            </AvatarFallback>
          </Avatar>
        )}
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <span className="truncate font-medium group-hover:underline">
              {project.name}
            </span>
            {health && health.status !== 'no_monitors' && (
              <HealthStatusDot status={health.status} />
            )}
          </div>
          <p className="truncate text-xs text-muted-foreground">
            {project.slug}
          </p>
        </div>
      </div>

      <div className="grid grid-cols-2 gap-x-6 gap-y-4 sm:grid-cols-3">
        <MetadataCell label="Preset">
          <div className="flex min-w-0 items-center gap-2 text-sm">
            {project.preset ? (
              <PresetIcon
                preset={project.preset}
                label={projectPresetLabel(project.preset)}
                className="size-6 shrink-0 rounded-md"
                imageClassName="p-1"
              />
            ) : (
              <span className="flex size-6 shrink-0 items-center justify-center rounded-md border bg-background text-muted-foreground">
                <PackageOpen className="size-3.5" />
              </span>
            )}
            <span className="truncate">
              {projectPresetLabel(project.preset)}
            </span>
          </div>
        </MetadataCell>

        <MetadataCell label="Repository">
          {repository ? (
            <div className="flex min-w-0 items-center gap-2 text-sm">
              <RepositoryIcon provider={repository.provider} />
              <span className="truncate font-mono text-xs">
                {repository.label}
              </span>
            </div>
          ) : (
            <span className="text-sm text-muted-foreground">No repository</span>
          )}
        </MetadataCell>

        <MetadataCell label="Activity · 24h">
          {analyticsLoading && apiTrafficLoading ? (
            <Skeleton className="h-5 w-28" />
          ) : analyticsError && apiTrafficError ? (
            <span className="inline-flex items-center gap-1.5 text-sm text-muted-foreground">
              <AlertCircle className="size-3.5" /> Unavailable
            </span>
          ) : (
            <div className="flex flex-wrap gap-x-3 gap-y-1 text-xs">
              {!apiTrafficError && (
                <span className="inline-flex items-center gap-1.5 whitespace-nowrap">
                  <Network className="size-3.5 text-muted-foreground" />
                  <strong className="font-semibold tabular-nums">
                    {apiTrafficLoading ? '…' : apiRequests.toLocaleString()}
                  </strong>
                  <span className="text-muted-foreground">API requests</span>
                </span>
              )}
              {!analyticsError && (
                <span className="inline-flex items-center gap-1.5 whitespace-nowrap">
                  <Users className="size-3.5 text-muted-foreground" />
                  <strong className="font-semibold tabular-nums">
                    {analyticsLoading ? '…' : totalVisitors.toLocaleString()}
                  </strong>
                  <span className="text-muted-foreground">visitors</span>
                </span>
              )}
            </div>
          )}
        </MetadataCell>
      </div>

      <MetadataCell label="Latest deployment">
        {project.last_deployment ? (
          <div className="min-w-0">
            <div className="flex items-center gap-2 text-sm">
              <Badge
                variant={
                  lastDeployment?.status === 'completed'
                    ? 'success'
                    : lastDeployment?.status === 'failed'
                      ? 'destructive'
                      : 'secondary'
                }
                className="h-5 px-1.5 capitalize"
              >
                {lastDeployment?.status ?? 'Checking'}
              </Badge>
            </div>
            <p className="mt-1 truncate text-xs text-muted-foreground">
              {deploymentLabel(lastDeployment?.status)}{' '}
              <TimeAgo date={project.last_deployment} />
            </p>
          </div>
        ) : (
          <span className="text-sm text-muted-foreground">Not deployed</span>
        )}
      </MetadataCell>

      <div className="hidden justify-self-end lg:block">
        {shortcutNumber !== undefined && (
          <KbdBadge keys={['⌃', shortcutNumber.toString()]} />
        )}
      </div>
    </Link>
  )
}
