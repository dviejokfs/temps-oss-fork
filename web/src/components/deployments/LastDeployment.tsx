import { DeploymentResponse } from '@/api/client'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { CopyButton } from '@/components/ui/copy-button'
import { TimeAgo } from '@/components/utils/TimeAgo'
import { ArrowRight, ExternalLink, GitBranch } from 'lucide-react'
import { Link } from 'react-router'
import { normalizeUrl } from '@/lib/deployment-url'
import { DeploymentStatusBadge } from '../deployment/DeploymentStatusBadge'

interface LastDeploymentProps {
  deployment: DeploymentResponse
  projectName: string
}

export function LastDeployment({
  deployment,
  projectName,
}: LastDeploymentProps) {
  const primaryUrl = deployment.environment.domains[0] ?? deployment.url
  const primaryHref = normalizeUrl(primaryUrl)

  return (
    <Card>
      <CardContent className="p-4 sm:p-5">
        <div className="flex flex-col gap-4 lg:flex-row lg:items-center lg:justify-between">
          <div className="min-w-0 space-y-3">
            <div className="flex flex-wrap items-center gap-2">
              <h3 className="text-sm font-semibold">Latest deployment</h3>
              <DeploymentStatusBadge deployment={deployment} />
              <span className="text-xs text-muted-foreground">
                <TimeAgo date={deployment.created_at} />
                {deployment.commit_author
                  ? ` by ${deployment.commit_author}`
                  : ''}
              </span>
            </div>
            <div className="min-w-0">
              <p className="truncate text-sm font-medium">
                {deployment.commit_message || 'Manual deployment'}
              </p>
              <p className="mt-1 flex items-center gap-1.5 text-xs text-muted-foreground">
                <GitBranch className="size-3.5 shrink-0" />
                <span className="truncate">
                  {deployment.branch || 'uploaded source'}
                  {deployment.commit_hash
                    ? ` · ${deployment.commit_hash.slice(0, 7)}`
                    : ''}
                </span>
              </p>
            </div>
            {primaryUrl && <DeploymentUrlRow value={primaryUrl} />}
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {primaryHref && (
              <Button variant="outline" size="sm" asChild>
                <a href={primaryHref} target="_blank" rel="noopener noreferrer">
                  Open site
                  <ExternalLink className="ml-1.5 size-3.5" />
                </a>
              </Button>
            )}
            <Button variant="ghost" size="sm" asChild>
              <Link
                to={`/projects/${projectName}/deployments/${deployment.id}`}
              >
                Details
                <ArrowRight className="ml-1.5 size-3.5" />
              </Link>
            </Button>
          </div>
        </div>
      </CardContent>
    </Card>
  )
}

function DeploymentUrlRow({ value }: { value: string }) {
  const href = normalizeUrl(value)
  return (
    <div className="flex min-w-0 items-center gap-1">
      {href ? (
        <>
          <a
            href={href}
            target="_blank"
            rel="noopener noreferrer"
            className="flex min-w-0 items-center gap-1 hover:opacity-80 transition-opacity"
          >
            <span className="truncate text-sm text-muted-foreground">
              {value}
            </span>
            <ExternalLink className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
          </a>
          <CopyButton
            value={href}
            minimal
            label="Copy URL"
            className="h-6 w-6 shrink-0 p-0 text-muted-foreground"
          />
        </>
      ) : (
        <span className="truncate text-sm text-muted-foreground">{value}</span>
      )}
    </div>
  )
}
