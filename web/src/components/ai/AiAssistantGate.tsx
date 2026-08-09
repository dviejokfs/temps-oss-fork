import { getCloudStatus, listProviderKeys } from '@/api/client'
import { Button } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'
import { useQuery } from '@tanstack/react-query'
import {
  ArrowRight,
  Cloud,
  Coins,
  ShieldCheck,
  Sparkles,
  X,
} from 'lucide-react'
import { type ReactNode } from 'react'
import { Link } from 'react-router'

export function AiAssistantGate({
  children,
  onClose,
}: {
  children: ReactNode
  onClose: () => void
}) {
  const providers = useQuery({
    queryKey: ['providerKeys'],
    queryFn: async () => (await listProviderKeys()).data ?? [],
    staleTime: 60_000,
    retry: false,
  })
  const cloud = useQuery({
    queryKey: ['cloudStatus', 'ai'],
    queryFn: async () => (await getCloudStatus()).data,
    staleTime: 15_000,
    retry: false,
  })

  if (providers.isPending || cloud.isPending) {
    return <AiDockSkeleton onClose={onClose} />
  }
  const hasLocalProvider = (providers.data ?? []).some((key) => key.is_active)
  const hasManagedProvider = cloud.data?.status === 'linked'
  if (!hasLocalProvider && !hasManagedProvider) {
    return <CloudAiEmptyState onClose={onClose} />
  }
  return children
}

function DockHeader({ onClose }: { onClose: () => void }) {
  return (
    <div className="flex items-center justify-between border-b border-border px-5 py-4">
      <div className="flex items-center gap-2">
        <span className="grid size-7 place-items-center rounded-md border border-border bg-muted/40">
          <Sparkles className="size-3.5" />
        </span>
        <span className="text-sm font-semibold">Temps AI</span>
      </div>
      <Button
        variant="ghost"
        size="icon"
        onClick={onClose}
        aria-label="Close AI assistant"
      >
        <X className="size-4" />
      </Button>
    </div>
  )
}

function AiDockSkeleton({ onClose }: { onClose: () => void }) {
  return (
    <div className="flex h-full flex-col">
      <DockHeader onClose={onClose} />
      <div className="mt-auto space-y-3 p-5">
        <Skeleton className="h-5 w-28" />
        <Skeleton className="h-16 w-full" />
        <Skeleton className="h-10 w-full" />
      </div>
    </div>
  )
}

export function CloudAiEmptyState({ onClose }: { onClose: () => void }) {
  return (
    <div className="flex h-full flex-col">
      <DockHeader onClose={onClose} />
      <div className="relative flex flex-1 flex-col overflow-hidden p-5">
        <div className="pointer-events-none absolute inset-x-0 top-0 h-48 bg-[radial-gradient(circle_at_80%_0%,hsl(var(--primary)/0.08),transparent_62%)]" />
        <div className="relative mt-auto space-y-6 pb-2">
          <div className="space-y-3">
            <p className="font-mono text-[10px] uppercase tracking-[0.16em] text-muted-foreground">
              Managed analysis
            </p>
            <h2 className="max-w-sm text-2xl font-semibold tracking-[-0.04em]">
              Ask your stack. Keep the evidence attached.
            </h2>
            <p className="max-w-md text-sm leading-6 text-muted-foreground">
              Connect Temps Cloud for evidence-backed explanations across
              traces, errors, analytics, and deploys. Local ingest and primary
              telemetry storage stay on this instance.
            </p>
          </div>

          <div className="divide-y divide-border border-y border-border">
            <AiCloudBenefit icon={<Coins />} title="250 AI credits included monthly">
              Credits reset each billing period. Extra usage stays off until
              you set a hard cap.
            </AiCloudBenefit>
            <AiCloudBenefit
              icon={<ShieldCheck />}
              title="Cited, read-only answers"
            >
              Conclusions link to the signals and time windows that support
              them.
            </AiCloudBenefit>
            <AiCloudBenefit icon={<Cloud />} title="Optional control plane">
              Connect in two steps without putting Cloud in your request path.
            </AiCloudBenefit>
          </div>

          <div className="grid gap-2">
            <Button asChild className="w-full justify-between">
              <Link to="/settings/cloud" onClick={onClose}>
                Connect Temps Cloud <ArrowRight className="size-4" />
              </Link>
            </Button>
            <Button
              asChild
              variant="ghost"
              className="w-full text-muted-foreground"
            >
              <Link to="/settings/ai-providers" onClick={onClose}>
                Use my own AI provider
              </Link>
            </Button>
          </div>
        </div>
      </div>
    </div>
  )
}

function AiCloudBenefit({
  icon,
  title,
  children,
}: {
  icon: ReactNode
  title: string
  children: ReactNode
}) {
  return (
    <div className="grid grid-cols-[28px_1fr] gap-3 py-3.5">
      <span className="mt-0.5 text-muted-foreground [&>svg]:size-4">
        {icon}
      </span>
      <div className="space-y-1">
        <p className="text-sm font-medium">{title}</p>
        <p className="text-xs leading-5 text-muted-foreground">{children}</p>
      </div>
    </div>
  )
}
