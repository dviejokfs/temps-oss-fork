import { useEffect, useMemo, useState } from 'react'
import { useBreadcrumbs } from '@/contexts/BreadcrumbContext'
import { useDashboardAnalytics } from '@/hooks/useDashboardAnalytics'
import { useDashboardHealth } from '@/hooks/useDashboardHealth'
import { usePageTitle } from '@/hooks/usePageTitle'
import { FirstProjectOnboarding } from '@/components/dashboard/FirstProjectOnboarding'
import { SIMULATE_EMPTY_INSTALL } from '@/lib/devSimulate'
import { ProjectCard } from '@/components/dashboard/ProjectCard'
import { ProjectCardSkeleton } from '@/components/skeletons/ProjectCardSkeleton'
import { Button } from '@/components/ui/button'
import { CreateActionButton } from '@/components/ui/create-action-button'
import {
  getApiTimeseriesOptions,
  getProjectsOptions,
  listGitProvidersOptions,
} from '@/api/client/@tanstack/react-query.gen'
import { useQueries, useQuery } from '@tanstack/react-query'
import { subDays } from 'date-fns'
import { ArrowRight, UploadCloud } from 'lucide-react'
import { Link, useNavigate } from 'react-router'
import { SourceLogo } from '@/components/imports/SourceLogo'
import {
  TOP_MIGRATION_SOURCES,
  importHref,
} from '@/components/imports/migration-sources'

const ITEMS_PER_PAGE = 9

export function Projects() {
  const { setBreadcrumbs } = useBreadcrumbs()
  const navigate = useNavigate()
  const [page, setPage] = useState(1)

  const { data: rawProjectsData, isLoading } = useQuery({
    ...getProjectsOptions({
      query: {
        page,
        per_page: ITEMS_PER_PAGE,
      },
    }),
  })

  const { data: rawGitProviders, isLoading: gitProvidersLoading } = useQuery({
    ...listGitProvidersOptions({}),
    retry: false,
  })

  // TEMP: force an empty (brand-new install) dashboard while iterating on the
  // first-run experience. See lib/devSimulate.ts.
  const projectsData = SIMULATE_EMPTY_INSTALL
    ? ({ ...rawProjectsData, projects: [], total: 0 } as typeof rawProjectsData)
    : rawProjectsData
  const gitProviders = SIMULATE_EMPTY_INSTALL ? [] : rawGitProviders

  useEffect(() => {
    setBreadcrumbs([{ label: 'Projects' }])
  }, [setBreadcrumbs])

  // Keyboard shortcut: N to create new project

  // Keyboard shortcuts: Ctrl+1 through Ctrl+9 to navigate to projects
  useEffect(() => {
    const handleKeyDown = (e: KeyboardEvent) => {
      // Check if user is typing in an input field
      const target = e.target as HTMLElement
      const isTyping =
        target.tagName === 'INPUT' ||
        target.tagName === 'TEXTAREA' ||
        target.isContentEditable

      // Only trigger if Ctrl (or Cmd on Mac) is pressed with a number key
      if (
        !isTyping &&
        (e.ctrlKey || e.metaKey) &&
        !e.altKey &&
        !e.shiftKey &&
        e.key >= '1' &&
        e.key <= '9'
      ) {
        const index = parseInt(e.key, 10) - 1
        const projects = projectsData?.projects || []

        if (projects[index]) {
          e.preventDefault()
          navigate(`/projects/${projects[index].slug}`)
        }
      }
    }

    document.addEventListener('keydown', handleKeyDown)
    return () => document.removeEventListener('keydown', handleKeyDown)
  }, [projectsData?.projects, navigate])

  usePageTitle('Projects')

  // Batch fetch analytics for all visible projects
  const { startDate, endDate } = useMemo(() => {
    return {
      startDate: subDays(new Date(), 1).toISOString(),
      endDate: new Date().toISOString(),
    }
  }, [])

  const projectIds = useMemo(
    () => projectsData?.projects?.map((p: { id: number }) => p.id) ?? [],
    [projectsData?.projects]
  )

  const dashboardAnalytics = useDashboardAnalytics(
    projectIds,
    startDate,
    endDate
  )

  const dashboardHealth = useDashboardHealth(projectIds)

  const apiTrafficQueries = useQueries({
    queries: projectIds.map((projectId) => ({
      ...getApiTimeseriesOptions({
        path: { project_id: projectId },
        query: {
          start_date: startDate,
          end_date: endDate,
        },
      }),
      staleTime: 30_000,
    })),
  })

  return (
    <div className="p-4 sm:p-8 space-y-6">
      {/* Header */}
      <ProjectsHeader
        actions={
          <>
            <PlatformStrip />
            <Button asChild variant="outline">
              <Link to="/drop">
                <UploadCloud className="mr-2 size-4" />
                Drop files
              </Link>
            </Button>
            <CreateActionButton to="/projects/new" label="New Project" />
          </>
        }
      />

      {isLoading || gitProvidersLoading ? (
        <div className="overflow-hidden rounded-xl border bg-card divide-y">
          {Array.from({ length: 4 }).map((_, i) => (
            <ProjectCardSkeleton key={i} />
          ))}
        </div>
      ) : projectsData?.projects.length === 0 ? (
        // First-run onboarding. The component is context-aware: when a Git
        // provider is already connected it routes straight into the import
        // wizard (skipping the connect step), and it always surfaces the
        // "deploy a project with a database" and CLI paths.
        <FirstProjectOnboarding
          gitConnected={!!gitProviders && gitProviders.length > 0}
        />
      ) : (
        <div className="overflow-hidden rounded-xl border bg-card">
          <div className="divide-y">
            {projectsData?.projects.map((project, index) => (
              <ProjectCard
                key={project.id}
                project={project}
                shortcutNumber={index < 9 ? index + 1 : undefined}
                analytics={
                  dashboardAnalytics.data?.projects?.[String(project.id)]
                }
                analyticsLoading={dashboardAnalytics.isLoading}
                analyticsError={dashboardAnalytics.isError}
                apiRequests={apiTrafficQueries[index]?.data?.total_requests}
                apiTrafficLoading={apiTrafficQueries[index]?.isPending}
                apiTrafficError={apiTrafficQueries[index]?.isError}
                health={dashboardHealth.data?.projects?.[String(project.id)]}
              />
            ))}
          </div>
        </div>
      )}

      {/* Pagination - Only show if there are projects */}
      {projectsData && projectsData.projects.length > 0 && (
        <div className="flex items-center justify-center gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => setPage((p) => Math.max(1, p - 1))}
            disabled={page === 1}
          >
            Previous
          </Button>
          <span className="text-sm text-muted-foreground">
            Page {page} of {Math.ceil(projectsData.total / ITEMS_PER_PAGE)}
          </span>
          <Button
            variant="outline"
            size="sm"
            onClick={() => setPage((p) => p + 1)}
            disabled={page >= Math.ceil(projectsData.total / ITEMS_PER_PAGE)}
          >
            Next
          </Button>
        </div>
      )}
    </div>
  )
}

/**
 * Projects page header. The title block is fixed; `actions` is what the
 * migration-entry-point variants swap out.
 */
function ProjectsHeader({ actions }: { actions: React.ReactNode }) {
  return (
    <div className="flex flex-col gap-3 sm:flex-row sm:justify-between sm:items-center">
      <div>
        <h1 className="text-2xl font-semibold tracking-tight">Projects</h1>
        <p className="text-sm text-muted-foreground">
          Manage your projects and their settings
        </p>
      </div>
      <div className="flex flex-wrap gap-2">{actions}</div>
    </div>
  )
}

/**
 * Migration entry point. The platforms themselves are the affordance: brand
 * marks sit inline in the header so someone arriving from Coolify or Dokploy
 * recognises the path instead of reading for it, and each mark deep-links the
 * import wizard with that source already selected — skipping its first step.
 */
function PlatformStrip() {
  return (
    <div className="flex items-center gap-1 rounded-md border p-1">
      {/* The label is the first thing to go when the header wraps on mobile —
          the brand marks still carry the meaning, and every one of them has an
          accessible name. */}
      <span className="hidden px-1.5 text-xs text-muted-foreground sm:inline">
        Migrate from
      </span>
      {TOP_MIGRATION_SOURCES.map((p) => (
        <Link
          key={p.source}
          to={importHref(p.source)}
          title={`Import from ${p.label}`}
          aria-label={`Import from ${p.label}`}
          className="rounded p-1.5 transition-colors hover:bg-accent"
        >
          <SourceLogo source={p.source} className="h-4 w-4" />
        </Link>
      ))}
      <Link
        to="/projects/import-wizard"
        className="rounded p-1.5 text-muted-foreground transition-colors hover:bg-accent"
        title="All platforms"
        aria-label="All platforms"
      >
        <ArrowRight className="h-4 w-4" />
      </Link>
    </div>
  )
}
