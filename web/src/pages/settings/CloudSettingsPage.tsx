import {
  disconnectCloudMutation,
  enrollCloudMutation,
  getCloudCapabilityOptions,
  getCloudStatusOptions,
} from '@/api/client/@tanstack/react-query.gen'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { useBreadcrumbs } from '@/contexts/BreadcrumbContext'
import { usePageTitle } from '@/hooks/usePageTitle'
import { zodResolver } from '@hookform/resolvers/zod'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  AlertCircle,
  Check,
  Cloud,
  Loader2,
  Radio,
  ShieldCheck,
  Unplug,
} from 'lucide-react'
import { useEffect } from 'react'
import type { ReactNode } from 'react'
import { useForm } from 'react-hook-form'
import { toast } from 'sonner'
import { z } from 'zod'

const enrollmentSchema = z.object({
  enrollmentCode: z
    .string()
    .trim()
    .min(1, 'Paste the enrollment code from Temps Cloud'),
})

type EnrollmentForm = z.infer<typeof enrollmentSchema>

export function CloudSettingsPage() {
  const { setBreadcrumbs } = useBreadcrumbs()
  const queryClient = useQueryClient()
  const capability = useQuery(getCloudCapabilityOptions())
  const status = useQuery({
    ...getCloudStatusOptions(),
    refetchInterval: 5_000,
  })
  const enroll = useMutation(enrollCloudMutation())
  const disconnect = useMutation(disconnectCloudMutation())
  const form = useForm<EnrollmentForm>({
    resolver: zodResolver(enrollmentSchema),
    defaultValues: { enrollmentCode: '' },
  })

  useEffect(() => {
    setBreadcrumbs([
      { label: 'Settings', href: '/settings' },
      { label: 'Temps Cloud' },
    ])
  }, [setBreadcrumbs])
  usePageTitle('Temps Cloud')

  const connected = status.data?.status === 'linked'
  const cloudConsoleUrl = status.data?.backend_url ?? 'https://app.temps.sh'
  const refresh = () =>
    Promise.all([
      queryClient.invalidateQueries({
        queryKey: getCloudStatusOptions().queryKey,
      }),
      queryClient.invalidateQueries({
        queryKey: getCloudCapabilityOptions().queryKey,
      }),
    ])

  const submit = form.handleSubmit(async ({ enrollmentCode }) => {
    try {
      await enroll.mutateAsync({ body: { enrollment_code: enrollmentCode } })
      form.reset()
      await refresh()
      toast.success('Instance connected to Temps Cloud')
    } catch (error) {
      toast.error(
        error instanceof Error
          ? error.message
          : 'Could not connect this instance'
      )
    }
  })

  const remove = async () => {
    try {
      await disconnect.mutateAsync({})
      await refresh()
      toast.success('Temps Cloud disconnected')
    } catch (error) {
      toast.error(
        error instanceof Error
          ? error.message
          : 'Could not disconnect this instance'
      )
    }
  }

  if (status.isLoading || capability.isLoading) {
    return (
      <div className="grid min-h-[420px] place-items-center">
        <Loader2 className="size-6 animate-spin text-muted-foreground" />
      </div>
    )
  }

  if (status.error) {
    return (
      <Alert variant="destructive">
        <AlertCircle className="size-4" />
        <AlertTitle>Temps Cloud status unavailable</AlertTitle>
        <AlertDescription>{status.error.message}</AlertDescription>
      </Alert>
    )
  }

  return (
    <div className="mx-auto max-w-5xl space-y-8 pb-12">
      <header className="border-b border-border pb-7">
        <div className="mb-3 flex items-center gap-2 font-mono text-[11px] uppercase tracking-[0.16em] text-muted-foreground">
          <Cloud className="size-3.5" /> Optional control plane
        </div>
        <h1 className="text-3xl font-semibold tracking-[-0.045em]">
          Temps Cloud
        </h1>
        <p className="mt-3 max-w-2xl text-sm leading-6 text-muted-foreground">
          See this instance alongside the rest of your fleet. Application
          traffic and primary telemetry storage stay on this machine.
        </p>
      </header>

      {!capability.data?.configured && (
        <Alert variant="destructive">
          <AlertCircle className="size-4" />
          <AlertTitle>Cloud connection needs configuration</AlertTitle>
          <AlertDescription>
            {capability.data?.reason ??
              'The managed backend is not configured.'}
          </AlertDescription>
        </Alert>
      )}

      {connected ? (
        <Card className="overflow-hidden border-border shadow-none">
          <div className="grid border-b border-border bg-muted/20 md:grid-cols-[1fr_auto] md:items-center">
            <div className="p-6">
              <p className="mb-2 font-mono text-[10px] uppercase tracking-[0.15em] text-muted-foreground">
                Connection state
              </p>
              <div className="flex items-center gap-3">
                <span className="grid size-9 place-items-center rounded-full border border-emerald-500/30 bg-emerald-500/10 text-emerald-500">
                  <Check className="size-4" />
                </span>
                <div>
                  <h2 className="font-semibold">Connected</h2>
                  <p className="text-sm text-muted-foreground">
                    {status.data?.status_message}
                  </p>
                </div>
              </div>
            </div>
            <div className="border-t border-border p-6 md:border-l md:border-t-0">
              <Button
                variant="outline"
                onClick={remove}
                disabled={disconnect.isPending}
              >
                <Unplug /> Disconnect
              </Button>
            </div>
          </div>
          <CardContent className="grid gap-px bg-border p-0 sm:grid-cols-3">
            <StatusCell
              label="Mirror health"
              value={status.data?.health ?? 'unknown'}
              detail={status.data?.health_message ?? ''}
            />
            <StatusCell
              label="Instance ID"
              value={status.data?.instance_id?.slice(0, 12) ?? '—'}
              detail="Stable across reconnections"
              mono
            />
            <StatusCell
              label="Buffered spans"
              value={String(status.data?.spooled_spans ?? 0)}
              detail="Local storage remains primary"
              mono
            />
          </CardContent>
        </Card>
      ) : (
        <div className="grid gap-8 lg:grid-cols-[minmax(0,1fr)_280px]">
          <Card className="border-border shadow-none">
            <CardContent className="p-6 md:p-8">
              <div className="mb-8 flex items-start justify-between gap-4">
                <div>
                  <p className="mb-2 font-mono text-[10px] uppercase tracking-[0.15em] text-muted-foreground">
                    Two-step setup
                  </p>
                  <h2 className="text-xl font-semibold tracking-tight">
                    Connect this instance
                  </h2>
                </div>
                <span className="rounded-full border border-border px-2.5 py-1 font-mono text-[10px] text-muted-foreground">
                  ~30 sec
                </span>
              </div>
              <form onSubmit={submit} className="space-y-5">
                <div className="space-y-2">
                  <div className="flex items-center justify-between">
                    <Label htmlFor="enrollment-code">
                      1. Paste enrollment code
                    </Label>
                    <a
                      href={cloudConsoleUrl}
                      target="_blank"
                      rel="noreferrer"
                      className="text-xs text-muted-foreground underline underline-offset-4 hover:text-foreground"
                    >
                      Get a code
                    </a>
                  </div>
                  <Input
                    id="enrollment-code"
                    autoFocus
                    placeholder="ABCD-EFGH"
                    className="h-12 font-mono uppercase tracking-[0.12em]"
                    aria-invalid={Boolean(form.formState.errors.enrollmentCode)}
                    {...form.register('enrollmentCode')}
                  />
                  {form.formState.errors.enrollmentCode && (
                    <p className="text-xs text-destructive">
                      {form.formState.errors.enrollmentCode.message}
                    </p>
                  )}
                </div>
                <Button
                  type="submit"
                  className="h-11 w-full"
                  disabled={enroll.isPending || !capability.data?.configured}
                >
                  {enroll.isPending ? (
                    <Loader2 className="animate-spin" />
                  ) : (
                    <Radio />
                  )}{' '}
                  2. Connect
                </Button>
              </form>
              <p className="mt-5 text-xs leading-5 text-muted-foreground">
                The code is exchanged for an instance-only credential stored
                with owner-only file permissions. Mirrored spans contain only
                trace and span IDs, a neutral operation label, timestamp, and
                duration. Application-controlled names and attributes are
                stripped; source code, environment variables, secrets, and
                application traffic never leave this instance.
              </p>
            </CardContent>
          </Card>

          <aside className="static h-auto border-0 bg-transparent p-0">
            <ol className="space-y-5">
              <TrustItem icon={<ShieldCheck />} title="Local stays primary">
                A Cloud outage never blocks ingest, deploys, or traffic.
              </TrustItem>
              <TrustItem icon={<Radio />} title="Background mirror">
                Accepted spans move through a bounded, non-blocking queue.
              </TrustItem>
              <TrustItem icon={<AlertCircle />} title="Visible failure">
                Buffering, dropped mirror data, and rejected credentials are
                explicit.
              </TrustItem>
            </ol>
          </aside>
        </div>
      )}
    </div>
  )
}

function StatusCell({
  label,
  value,
  detail,
  mono = false,
}: {
  label: string
  value: string
  detail: string
  mono?: boolean
}) {
  return (
    <div className="bg-card p-5">
      <p className="text-xs text-muted-foreground">{label}</p>
      <p
        className={`mt-2 text-sm font-semibold capitalize ${mono ? 'font-mono' : ''}`}
      >
        {value.split('_').join(' ')}
      </p>
      <p className="mt-1 text-[11px] leading-4 text-muted-foreground">
        {detail}
      </p>
    </div>
  )
}

function TrustItem({
  icon,
  title,
  children,
}: {
  icon: ReactNode
  title: string
  children: ReactNode
}) {
  return (
    <li className="grid grid-cols-[28px_1fr] gap-3 border-t border-border pt-4 first:border-0 first:pt-0">
      <span className="mt-0.5 text-muted-foreground [&>svg]:size-4">
        {icon}
      </span>
      <div>
        <h3 className="text-sm font-medium">{title}</h3>
        <p className="mt-1 text-xs leading-5 text-muted-foreground">
          {children}
        </p>
      </div>
    </li>
  )
}
