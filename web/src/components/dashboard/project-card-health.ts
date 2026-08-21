/**
 * Health indicator shown on every project card.
 *
 * The backend derives `status` from the proxy's error rate over the requested
 * window, and reports `"unknown"` for a project that received no requests at
 * all. Those are two very different things to a user — "we measured it and it
 * is fine" versus "nothing reached it, so there is nothing to measure" — and
 * neither of them may render as *nothing*. A card that silently drops its
 * indicator teaches the operator that Temps has no health signal.
 */

export type ProjectHealthTone =
  'healthy' | 'degraded' | 'down' | 'idle' | 'unavailable' | 'pending'

export interface ProjectHealthIndicator {
  tone: ProjectHealthTone
  /** Short label rendered next to the dot. */
  label: string
  /** Full sentence explaining what the label means and how it was derived. */
  detail: string
}

/** The slice of `ProjectHealthSummary` this indicator reads. */
export interface ProjectHealthInput {
  status: string
  total_requests: number
  total_errors: number
  error_rate: number
  avg_response_time_ms: number
}

export interface ProjectHealthIndicatorOptions {
  health?: ProjectHealthInput
  loading?: boolean
  error?: boolean
  /** Hours the summary covers, for the explanatory detail text. */
  windowHours?: number
}

function windowLabel(hours: number): string {
  if (hours === 24) return 'the last 24 hours'
  if (hours === 1) return 'the last hour'
  return `the last ${hours} hours`
}

function measuredDetail(
  health: ProjectHealthInput,
  windowHours: number,
  lead: string
): string {
  const requests = health.total_requests.toLocaleString()
  const errors = health.total_errors.toLocaleString()
  const latency = Math.round(health.avg_response_time_ms).toLocaleString()
  return (
    `${lead} ${requests} requests over ${windowLabel(windowHours)}, ` +
    `${errors} server errors (${health.error_rate}%), ${latency} ms average response time.`
  )
}

export function projectHealthIndicator({
  health,
  loading = false,
  error = false,
  windowHours = 24,
}: ProjectHealthIndicatorOptions): ProjectHealthIndicator {
  // An outright failure is reported as a failure. Falling back to a grey dot
  // would read as "this project is quiet" when we simply could not ask.
  if (error) {
    return {
      tone: 'unavailable',
      // Short enough to sit beside a long project name; the detail carries the
      // rest, and is exposed to screen readers rather than hidden in a tooltip.
      label: 'Unavailable',
      detail:
        'Temps could not load request health for this project. Reload the page to try again; if it persists, check that the proxy is running.',
    }
  }

  if (loading && !health) {
    return {
      tone: 'pending',
      label: 'Checking…',
      detail: `Loading request health for ${windowLabel(windowHours)}.`,
    }
  }

  if (!health) {
    return {
      tone: 'unavailable',
      label: 'No health data',
      detail:
        'The health summary returned no entry for this project, so its status could not be determined.',
    }
  }

  // Zero requests is a real, reportable state — not a missing one. The backend
  // labels it "unknown" because it cannot compute an error rate from nothing.
  if (health.total_requests === 0) {
    return {
      tone: 'idle',
      label: 'No traffic',
      detail: `No requests reached this project in ${windowLabel(windowHours)}, so there is no health signal to report yet.`,
    }
  }

  switch (health.status) {
    case 'healthy':
    case 'operational':
      return {
        tone: 'healthy',
        label: 'Healthy',
        detail: measuredDetail(health, windowHours, 'Serving normally:'),
      }
    case 'degraded':
      return {
        tone: 'degraded',
        label: 'Degraded',
        detail: measuredDetail(
          health,
          windowHours,
          'Elevated error rate over 10%:'
        ),
      }
    case 'down':
      return {
        tone: 'down',
        label: 'Down',
        detail: measuredDetail(
          health,
          windowHours,
          'More than half of requests are failing:'
        ),
      }
    default:
      // A status this build does not know about still has real traffic behind
      // it, so report the numbers rather than pretending the project is idle.
      return {
        tone: 'unavailable',
        label: 'Unknown',
        detail: measuredDetail(
          health,
          windowHours,
          `Unrecognized health status "${health.status}".`
        ),
      }
  }
}
