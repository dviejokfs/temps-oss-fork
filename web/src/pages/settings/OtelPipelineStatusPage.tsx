// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { Skeleton } from '@/components/ui/skeleton'
import { useBreadcrumbs } from '@/contexts/BreadcrumbContext'
import { usePageTitle } from '@/hooks/usePageTitle'
import { getPipelineStatsOptions } from '@/api/client/@tanstack/react-query.gen'
import { useQuery } from '@tanstack/react-query'
import { AlertCircle, AlertTriangle, ArrowRight, Activity } from 'lucide-react'
import { useEffect } from 'react'
import { Link } from 'react-router'

function StatCard({
  label,
  value,
  warn,
  description,
}: {
  label: string
  value: number | undefined
  warn?: boolean
  description?: string
}) {
  return (
    <div
      className={`rounded-lg border p-4 ${warn && value ? 'border-amber-400 bg-amber-50 dark:bg-amber-950/30' : 'bg-card'}`}
    >
      <p className="text-xs font-medium text-muted-foreground uppercase tracking-wide">
        {label}
      </p>
      {value === undefined ? (
        <Skeleton className="mt-2 h-7 w-20" />
      ) : (
        <p
          className={`mt-1 text-2xl font-semibold tabular-nums ${warn && value > 0 ? 'text-amber-600 dark:text-amber-400' : ''}`}
        >
          {value.toLocaleString()}
        </p>
      )}
      {description && (
        <p className="mt-1 text-xs text-muted-foreground">{description}</p>
      )}
    </div>
  )
}

function SignalSection({
  label,
  received,
  stored,
  dropped,
  isLoading,
}: {
  label: string
  received: number | undefined
  stored: number | undefined
  dropped: number | undefined
  isLoading: boolean
}) {
  const droppedCount = dropped ?? 0
  return (
    <div className="space-y-2">
      <div className="flex items-center gap-2">
        <h3 className="text-sm font-medium">{label}</h3>
        {!isLoading && droppedCount > 0 && (
          <Badge variant="destructive" className="text-xs">
            {droppedCount.toLocaleString()} dropped
          </Badge>
        )}
      </div>
      <div className="grid grid-cols-2 md:grid-cols-3 gap-3">
        <StatCard
          label="Received"
          value={isLoading ? undefined : received}
          description="Total ingest requests"
        />
        <StatCard
          label="Stored"
          value={isLoading ? undefined : stored}
          description="Successfully persisted"
        />
        <StatCard
          label="Dropped"
          value={isLoading ? undefined : dropped}
          description="Failed to store"
        />
      </div>
    </div>
  )
}

export function OtelPipelineStatusPage() {
  const { setBreadcrumbs } = useBreadcrumbs()

  useEffect(() => {
    setBreadcrumbs([
      { label: 'Settings', href: '/settings' },
      { label: 'OTel Pipeline Status' },
    ])
  }, [setBreadcrumbs])

  usePageTitle('OTel Pipeline Status')

  const { data, isLoading, error } = useQuery({
    ...getPipelineStatsOptions({ cache: 'no-store' }),
    // Matches NodesPage's status-tile cadence: frequent enough that an
    // operator watching this page after a rejection spike sees it clear
    // without a manual refresh, cheap enough for a page that's rarely open.
    refetchInterval: 30_000,
  })

  const stats = data?.stats

  const rateLimited = stats?.rate_limited_requests ?? 0
  const quotaExceeded = stats?.quota_exceeded_requests ?? 0
  const hasRejections = !isLoading && (rateLimited > 0 || quotaExceeded > 0)

  if (error) {
    return (
      <Alert variant="destructive">
        <AlertCircle className="h-4 w-4" />
        <AlertTitle>Failed to load pipeline stats</AlertTitle>
        <AlertDescription>
          Could not fetch OTel pipeline statistics. The server may be
          unavailable or you may not have permission.
        </AlertDescription>
      </Alert>
    )
  }

  return (
    <div className="space-y-6">
      <div className="flex flex-col gap-1">
        <div className="flex items-center gap-2">
          <Activity className="h-5 w-5 text-muted-foreground" />
          <h1 className="text-xl font-semibold">OTel Pipeline Status</h1>
        </div>
        <p className="text-sm text-muted-foreground">
          Cumulative counters since the last server restart. Rejection counters
          are also written to the metrics store every 60&nbsp;s and can trigger
          alarms.
        </p>
      </div>

      {/* Rejection counters — always shown first since they're the point of this page */}
      <Card className={hasRejections ? 'border-amber-400' : undefined}>
        <CardHeader className="pb-3">
          <div className="flex items-center gap-2">
            <CardTitle className="text-base">Rejected requests</CardTitle>
            {hasRejections && (
              <AlertTriangle className="h-4 w-4 text-amber-500" />
            )}
          </div>
          <CardDescription>
            Ingest requests turned away since the server started. Non-zero
            values mean projects are hitting their quotas or rate limits.
          </CardDescription>
        </CardHeader>
        <CardContent>
          <div className="grid grid-cols-2 md:grid-cols-4 gap-3">
            <StatCard
              label="Rate limited (429)"
              value={isLoading ? undefined : rateLimited}
              warn
              description="otel.rate_limited_requests"
            />
            <StatCard
              label="Quota exceeded (413)"
              value={isLoading ? undefined : quotaExceeded}
              warn
              description="otel.quota_exceeded_requests"
            />
          </div>

          {hasRejections && (
            <Alert className="mt-4" variant="default">
              <AlertTriangle className="h-4 w-4 text-amber-500" />
              <AlertTitle>Rejections detected</AlertTitle>
              <AlertDescription className="flex items-center gap-2">
                Projects are being rate-limited or have exceeded their storage
                quota. Check the{' '}
                <Link
                  to="/monitoring/alarms"
                  className="inline-flex items-center gap-1 font-medium underline underline-offset-2"
                >
                  Alarms page
                  <ArrowRight className="h-3 w-3" />
                </Link>{' '}
                to see whether the OtelRateLimited alarm is firing.
              </AlertDescription>
            </Alert>
          )}

          {!hasRejections && !isLoading && (
            <p className="mt-3 text-xs text-muted-foreground">
              No rejections recorded.{' '}
              <Link
                to="/monitoring/alarms"
                className="inline-flex items-center gap-1 underline underline-offset-2"
              >
                View alarms
                <ArrowRight className="h-3 w-3" />
              </Link>
            </p>
          )}
        </CardContent>
      </Card>

      {/* Per-signal pipeline health */}
      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="text-base">Pipeline throughput</CardTitle>
          <CardDescription>
            Received vs. stored vs. dropped counts per signal type since the
            last server restart.
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-6">
          <SignalSection
            label="Traces (spans)"
            received={stats?.spans_received}
            stored={stats?.spans_stored}
            dropped={stats?.spans_dropped}
            isLoading={isLoading}
          />
          <SignalSection
            label="Metrics"
            received={stats?.metrics_received}
            stored={stats?.metrics_stored}
            dropped={stats?.metrics_dropped}
            isLoading={isLoading}
          />
          <SignalSection
            label="Logs"
            received={stats?.logs_received}
            stored={stats?.logs_stored_db}
            dropped={stats?.logs_dropped}
            isLoading={isLoading}
          />

          <div className="border-t pt-4">
            <div className="flex items-center gap-2">
              <h3 className="text-sm font-medium">Ingest errors</h3>
              {!isLoading && (stats?.ingest_errors ?? 0) > 0 && (
                <Badge variant="destructive" className="text-xs">
                  {(stats?.ingest_errors ?? 0).toLocaleString()} errors
                </Badge>
              )}
            </div>
            <div className="mt-2 grid grid-cols-2 md:grid-cols-4 gap-3">
              <StatCard
                label="Ingest errors"
                value={isLoading ? undefined : stats?.ingest_errors}
                description="Parse/processing failures"
              />
            </div>
          </div>
        </CardContent>
      </Card>
    </div>
  )
}
