import { useEffect } from 'react'
import { Link } from 'react-router'
import { useQuery } from '@tanstack/react-query'
import {
  ArrowLeft,
  ArrowRight,
  Bot,
  Check,
  Circle,
  ExternalLink,
  MessageSquare,
  Sparkles,
  Terminal,
  Wand2,
} from 'lucide-react'
import {
  getAiProviderStatusOptions,
  listGlobalSkillsOptions,
} from '@/api/client/@tanstack/react-query.gen'
import { Button } from '@/components/ui/button'
import { Card, CardContent } from '@/components/ui/card'
import { CopyButton } from '@/components/ui/copy-button'
import { Skeleton } from '@/components/ui/skeleton'
import { useBreadcrumbs } from '@/contexts/BreadcrumbContext'
import { usePageTitle } from '@/hooks/usePageTitle'
import { buildAiOnboardingCommands } from '@/lib/ai-onboarding'
import { cn } from '@/lib/utils'

export function AiOnboarding() {
  const { setBreadcrumbs } = useBreadcrumbs()
  const origin = typeof window === 'undefined' ? '' : window.location.origin
  const commands = buildAiOnboardingCommands(origin)

  const { data: providerStatus, isLoading: providerLoading } = useQuery({
    ...getAiProviderStatusOptions(),
    retry: false,
  })
  const { data: skillsData, isLoading: skillsLoading } = useQuery({
    ...listGlobalSkillsOptions(),
    retry: false,
  })

  usePageTitle('Start using AI')

  useEffect(() => {
    setBreadcrumbs([
      { label: 'Finish setting up', href: '/setup' },
      { label: 'Start using AI' },
    ])
  }, [setBreadcrumbs])

  const providerReady = providerStatus?.configured === true
  const globalSkillCount = skillsData?.items?.length ?? 0
  const providerSetupPath = providerStatus?.setup_path || '/ai-gateway'

  return (
    <div className="mx-auto w-full max-w-6xl space-y-6 p-4 sm:p-8">
      <div className="flex items-center gap-3">
        <Button asChild variant="ghost" size="icon" className="shrink-0">
          <Link to="/setup" aria-label="Back to setup">
            <ArrowLeft className="size-4" />
          </Link>
        </Button>
        <div>
          <p className="text-xs font-medium uppercase tracking-[0.18em] text-primary">
            AI quickstart
          </p>
          <h1 className="mt-1 text-2xl font-semibold tracking-tight sm:text-3xl">
            Make Temps useful to your AI
          </h1>
          <p className="mt-1 max-w-2xl text-sm text-muted-foreground sm:text-base">
            Connect a model once, teach your coding agent how Temps works, and
            authenticate Bunx against this instance.
          </p>
        </div>
      </div>

      <Card className="overflow-hidden border-primary/20">
        <CardContent className="relative p-0">
          <div
            className="absolute inset-0 opacity-80"
            aria-hidden="true"
            style={{
              backgroundImage:
                'radial-gradient(80% 120% at 0% 0%, color-mix(in oklch, var(--primary) 12%, transparent) 0%, transparent 62%)',
            }}
          />
          <div className="relative grid gap-6 p-5 sm:p-7 lg:grid-cols-[1fr_auto] lg:items-center">
            <div className="flex items-start gap-4">
              <div className="flex size-11 shrink-0 items-center justify-center rounded-xl bg-primary text-primary-foreground shadow-sm">
                <Sparkles className="size-5" />
              </div>
              <div>
                <h2 className="text-lg font-semibold tracking-tight">
                  Your first useful AI loop
                </h2>
                <p className="mt-1 max-w-2xl text-sm leading-relaxed text-muted-foreground">
                  Ask your agent to prepare an app for Temps, deploy it through
                  Bunx, then use Temps AI to investigate the resulting errors,
                  traces, and traffic.
                </p>
              </div>
            </div>
            {providerLoading ? (
              <Skeleton className="h-10 w-40" />
            ) : providerReady ? (
              <Button asChild size="lg">
                <Link to="/chat">
                  Open AI chat
                  <MessageSquare className="ml-1.5 size-4" />
                </Link>
              </Button>
            ) : (
              <Button asChild size="lg">
                <Link to={providerSetupPath}>
                  Connect AI provider
                  <ArrowRight className="ml-1.5 size-4" />
                </Link>
              </Button>
            )}
          </div>
        </CardContent>
      </Card>

      <div className="grid gap-4 lg:grid-cols-3">
        <OnboardingStep
          number="01"
          icon={Bot}
          title="Connect a model"
          description="Choose a subscription CLI or bring an API key. This powers chat, autofix, summaries, and workflows across Temps."
          ready={providerReady}
          loading={providerLoading}
          status={providerReady ? 'AI ready' : 'Required'}
        >
          {!providerLoading && (
            <div className="space-y-3">
              {!providerReady && providerStatus?.reason && (
                <p className="rounded-lg border border-amber-500/20 bg-amber-500/5 px-3 py-2 text-xs leading-relaxed text-amber-700 dark:text-amber-300">
                  {providerStatus.reason}
                </p>
              )}
              <Button asChild variant={providerReady ? 'outline' : 'default'}>
                <Link to={providerSetupPath}>
                  {providerReady ? 'Manage provider' : 'Choose provider'}
                  <ArrowRight className="ml-1.5 size-3.5" />
                </Link>
              </Button>
            </div>
          )}
        </OnboardingStep>

        <OnboardingStep
          number="02"
          icon={Wand2}
          title="Give your agent Temps skills"
          description="Install the maintained Temps skill pack in your app so Codex, Claude Code, Cursor, and compatible agents know the right setup steps."
          ready={globalSkillCount > 0}
          loading={skillsLoading}
          status={
            globalSkillCount > 0
              ? `${globalSkillCount} workflow skill${globalSkillCount === 1 ? '' : 's'}`
              : 'Recommended'
          }
        >
          <div className="space-y-3">
            <Command value={commands.installSkills} />
            <div className="flex flex-wrap items-center gap-3 text-xs">
              <Link
                to="/skills"
                className="inline-flex items-center gap-1 font-medium text-primary hover:underline"
              >
                Add a skill to Temps workflows
                <ArrowRight className="size-3" />
              </Link>
              <a
                href="https://temps.sh/docs/features/skills"
                target="_blank"
                rel="noreferrer"
                className="inline-flex items-center gap-1 text-muted-foreground hover:text-foreground"
              >
                Skill catalog
                <ExternalLink className="size-3" />
              </a>
            </div>
          </div>
        </OnboardingStep>

        <OnboardingStep
          number="03"
          icon={Terminal}
          title="Connect Bunx to this instance"
          description="Run one device-login command in your terminal. Bunx downloads the CLI on demand, so there is nothing to install globally."
          status="One command"
        >
          <div className="space-y-3">
            <Command value={commands.connectCli} />
            <p className="text-xs leading-relaxed text-muted-foreground">
              The browser will ask you to approve the terminal. Credentials stay
              on your machine and are scoped to this Temps instance.
            </p>
          </div>
        </OnboardingStep>
      </div>

      <div className="flex flex-col gap-3 rounded-xl border bg-muted/30 p-4 sm:flex-row sm:items-center sm:justify-between">
        <div>
          <p className="text-sm font-medium">You can return here at any time</p>
          <p className="mt-0.5 text-xs text-muted-foreground">
            Finish setup keeps this AI quickstart visible until a provider is
            ready.
          </p>
        </div>
        <Button asChild variant="outline">
          <Link to="/setup">Back to all setup steps</Link>
        </Button>
      </div>
    </div>
  )
}

function OnboardingStep({
  number,
  icon: Icon,
  title,
  description,
  ready = false,
  loading = false,
  status,
  children,
}: {
  number: string
  icon: typeof Bot
  title: string
  description: string
  ready?: boolean
  loading?: boolean
  status: string
  children: React.ReactNode
}) {
  return (
    <Card className="h-full overflow-hidden">
      <CardContent className="flex h-full flex-col p-5">
        <div className="flex items-start justify-between gap-3">
          <div className="flex items-center gap-3">
            <div
              className={cn(
                'flex size-9 items-center justify-center rounded-lg border',
                ready
                  ? 'border-emerald-500/30 bg-emerald-500/10 text-emerald-500'
                  : 'bg-muted text-muted-foreground'
              )}
            >
              {ready ? (
                <Check className="size-4" />
              ) : (
                <Icon className="size-4" />
              )}
            </div>
            <span className="font-mono text-[11px] text-muted-foreground">
              {number}
            </span>
          </div>
          {loading ? (
            <Skeleton className="h-5 w-20" />
          ) : (
            <span
              className={cn(
                'inline-flex items-center gap-1.5 rounded-full border px-2 py-1 text-[10px] font-medium uppercase tracking-wide',
                ready
                  ? 'border-emerald-500/30 bg-emerald-500/5 text-emerald-600 dark:text-emerald-400'
                  : 'text-muted-foreground'
              )}
            >
              {ready ? (
                <Check className="size-3" />
              ) : (
                <Circle className="size-2.5" />
              )}
              {status}
            </span>
          )}
        </div>
        <h2 className="mt-5 text-base font-semibold tracking-tight">{title}</h2>
        <p className="mt-1 min-h-16 text-sm leading-relaxed text-muted-foreground">
          {description}
        </p>
        <div className="mt-auto pt-5">{children}</div>
      </CardContent>
    </Card>
  )
}

function Command({ value }: { value: string }) {
  return (
    <div className="flex min-w-0 items-center gap-2 rounded-lg border bg-background px-3 py-2.5">
      <code className="min-w-0 flex-1 overflow-x-auto whitespace-nowrap font-mono text-xs">
        {value}
      </code>
      <CopyButton
        value={value}
        minimal
        className="size-7 shrink-0 rounded-md"
        label="Copy command"
      />
    </div>
  )
}
