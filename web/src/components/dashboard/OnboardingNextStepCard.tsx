import { Button } from '@/components/ui/button'
import { useGettingStarted } from '@/hooks/useGettingStarted'
import {
  ArrowRight,
  Bell,
  Bot,
  Database,
  DatabaseBackup,
  GitBranch,
  Globe,
  ShieldCheck,
  Sparkles,
  Users,
  type LucideIcon,
} from 'lucide-react'
import { Link } from 'react-router'
import { nextIncompleteGettingStartedItem } from './onboarding-next-step'

const STEP_ICONS: Record<string, LucideIcon> = {
  ai: Bot,
  git: GitBranch,
  domain: Globe,
  notifications: Bell,
  dns: ShieldCheck,
  database: Database,
  backups: DatabaseBackup,
  team: Users,
}

export function OnboardingNextStepCard() {
  const { items, completedCount, totalCount, visible } = useGettingStarted()
  const nextStep = nextIncompleteGettingStartedItem(items)

  if (!visible || !nextStep) return null

  const Icon = STEP_ICONS[nextStep.key] ?? Sparkles

  return (
    <section
      aria-labelledby="dashboard-onboarding-title"
      className="rounded-xl border border-primary/20 bg-card px-4 py-3"
    >
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center">
        <div className="flex min-w-0 flex-1 items-start gap-3 sm:items-center">
          <div className="flex size-9 shrink-0 items-center justify-center rounded-lg border bg-muted/50 text-primary">
            <Icon className="size-4" />
          </div>

          <div className="min-w-0">
            <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
              <span className="text-[11px] font-medium uppercase tracking-wider text-primary">
                Up next · {completedCount + 1}/{totalCount}
              </span>
              <h2
                id="dashboard-onboarding-title"
                className="text-sm font-semibold"
              >
                {nextStep.label}
              </h2>
            </div>
            <p className="mt-0.5 text-xs leading-5 text-muted-foreground sm:truncate">
              {nextStep.description}
            </p>
          </div>
        </div>

        <div className="flex shrink-0 items-center gap-2 pl-12 sm:pl-0">
          <Button asChild variant="ghost" size="sm" className="text-xs">
            <Link to="/setup">View checklist</Link>
          </Button>
          <Button asChild size="sm">
            <Link to={nextStep.href}>
              {nextStep.cta}
              <ArrowRight className="size-3.5" />
            </Link>
          </Button>
        </div>
      </div>
    </section>
  )
}
