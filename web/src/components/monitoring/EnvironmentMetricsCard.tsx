import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  ChartConfig,
  ChartContainer,
  ChartTooltip,
  ChartTooltipContent,
  ChartLegend,
  ChartLegendContent,
} from '@/components/ui/chart'
import { Line, LineChart, XAxis, YAxis, CartesianGrid } from 'recharts'
import {
  ContainerInfoResponse,
  EnvironmentResponse,
} from '@/api/client/types.gen'
import { Skeleton } from '@/components/ui/skeleton'
import { format } from 'date-fns'

interface MetricsSnapshot {
  cpu_percent: number
  memory_bytes: number
  memory_limit_bytes: number | null
  memory_percent: number | null
  network_rx_bytes: number
  network_tx_bytes: number
  timestamp: string
  container_id: string
  container_name: string
}

export interface AggregatedMetrics {
  cpu: number
  memoryMb: number
  memoryPercent: number
  networkRxKBs: number
  networkTxKBs: number
}

export interface ChartDataPoint {
  time: string
  timestamp: number
  networkRx: number
  networkTx: number
  [seriesKey: string]: number | string
}

const MAX_DATA_POINTS = 120

// Theme only defines 5 chart colors (web/src/globals.css); beyond 5 replicas
// colors repeat but the legend label (container name) still disambiguates.
const CHART_COLORS = [
  'var(--chart-1)',
  'var(--chart-2)',
  'var(--chart-3)',
  'var(--chart-4)',
  'var(--chart-5)',
]

interface ContainerSeries {
  key: string
  label: string
  color: string
}

function formatNetworkRate(kbs: number): string {
  if (kbs >= 1024) return `${(kbs / 1024).toFixed(1)} MB/s`
  if (kbs >= 1) return `${kbs.toFixed(1)} KB/s`
  return `${(kbs * 1024).toFixed(0)} B/s`
}

const networkChartConfig = {
  networkRx: {
    label: 'Received',
    color: 'var(--chart-3)',
  },
  networkTx: {
    label: 'Sent',
    color: 'var(--chart-5)',
  },
} satisfies ChartConfig

interface EnvironmentMetricsChartsProps {
  projectId: number
  environment: EnvironmentResponse
  containers: ContainerInfoResponse[]
  onMetricsUpdate?: (metrics: AggregatedMetrics | null) => void
}

export function EnvironmentMetricsCharts({
  projectId,
  environment,
  containers,
  onMetricsUpdate,
}: EnvironmentMetricsChartsProps) {
  const metricsMapRef = useRef<Map<string, MetricsSnapshot>>(new Map())
  const prevNetworkRef = useRef<{ rx: number; tx: number; ts: number } | null>(
    null
  )
  const [chartHistory, setChartHistory] = useState<ChartDataPoint[]>([])
  const [connectedCount, setConnectedCount] = useState(0)
  const hasDataRef = useRef(false)
  const [hasData, setHasData] = useState(false)
  const [liveMetrics, setLiveMetrics] = useState<AggregatedMetrics | null>(null)
  const eventSourcesRef = useRef<Map<string, EventSource>>(new Map())

  const activeContainers = useMemo(
    () => containers.filter((c) => c.status === 'running'),
    [containers]
  )

  // Stable per-container series identity (color + legend label), keyed by a
  // short positional id rather than the container id itself so it stays a
  // safe CSS-custom-property / recharts dataKey suffix.
  const containerSeries = useMemo(() => {
    const sorted = [...activeContainers].sort((a, b) =>
      a.container_id.localeCompare(b.container_id)
    )
    const map = new Map<string, ContainerSeries>()
    sorted.forEach((container, index) => {
      map.set(container.container_id, {
        key: `c${index}`,
        label: (container.container_name || container.container_id).replace(
          /^\//,
          ''
        ),
        color: CHART_COLORS[index % CHART_COLORS.length],
      })
    })
    return map
  }, [activeContainers])

  const cpuChartConfig = useMemo(() => {
    const config: ChartConfig = {}
    for (const series of containerSeries.values()) {
      config[`cpu_${series.key}`] = { label: series.label, color: series.color }
    }
    return config
  }, [containerSeries])

  const memoryChartConfig = useMemo(() => {
    const config: ChartConfig = {}
    for (const series of containerSeries.values()) {
      config[`mem_${series.key}`] = { label: series.label, color: series.color }
    }
    return config
  }, [containerSeries])

  // Store onMetricsUpdate in a ref so callbacks don't depend on it. Updated
  // in an effect, not during render, since refs aren't meant to be written
  // while rendering.
  const onMetricsUpdateRef = useRef(onMetricsUpdate)
  useEffect(() => {
    onMetricsUpdateRef.current = onMetricsUpdate
  })

  const aggregateMetrics = useCallback((): AggregatedMetrics | null => {
    const allMetrics = Array.from(metricsMapRef.current.values())
    if (allMetrics.length === 0) return null

    const totalCpu = allMetrics.reduce(
      (sum, m) => sum + (m.cpu_percent ?? 0),
      0
    )
    const totalMemory = allMetrics.reduce(
      (sum, m) => sum + (m.memory_bytes ?? 0),
      0
    )
    const totalMemoryLimit = allMetrics.reduce(
      (sum, m) => sum + (m.memory_limit_bytes ?? 0),
      0
    )
    const memoryPercent =
      totalMemoryLimit > 0 ? (totalMemory / totalMemoryLimit) * 100 : 0
    const memoryMb = totalMemory / (1024 * 1024)

    // Compute network rate (KB/s) from cumulative byte counters
    const totalRx = allMetrics.reduce(
      (sum, m) => sum + (m.network_rx_bytes ?? 0),
      0
    )
    const totalTx = allMetrics.reduce(
      (sum, m) => sum + (m.network_tx_bytes ?? 0),
      0
    )
    const now = Date.now()
    let networkRxKBs = 0
    let networkTxKBs = 0
    const prev = prevNetworkRef.current
    if (prev) {
      const elapsed = (now - prev.ts) / 1000 // seconds
      if (elapsed > 0) {
        networkRxKBs = Math.max(0, (totalRx - prev.rx) / 1024 / elapsed)
        networkTxKBs = Math.max(0, (totalTx - prev.tx) / 1024 / elapsed)
      }
    }
    prevNetworkRef.current = { rx: totalRx, tx: totalTx, ts: now }

    return {
      cpu: Math.round(totalCpu * 100) / 100,
      memoryMb: Math.round(memoryMb * 100) / 100,
      memoryPercent: Math.round(memoryPercent * 100) / 100,
      networkRxKBs: Math.round(networkRxKBs * 100) / 100,
      networkTxKBs: Math.round(networkTxKBs * 100) / 100,
    }
  }, [])

  // Per-replica CPU/memory values for the current tick, keyed by each
  // container's series id so each replica renders as its own labeled line
  // instead of being folded into a single summed line.
  const buildPerContainerPoint = useCallback((): Record<
    string,
    number
  > | null => {
    if (containerSeries.size === 0) return null
    const point: Record<string, number> = {}
    for (const [containerId, series] of containerSeries) {
      const m = metricsMapRef.current.get(containerId)
      if (!m) continue
      point[`cpu_${series.key}`] = Math.round((m.cpu_percent ?? 0) * 100) / 100
      point[`mem_${series.key}`] =
        Math.round(((m.memory_bytes ?? 0) / (1024 * 1024)) * 100) / 100
    }
    return point
  }, [containerSeries])

  const connectToStream = useCallback(
    (container: ContainerInfoResponse) => {
      const existing = eventSourcesRef.current.get(container.container_id)
      if (existing) {
        existing.close()
      }

      const eventSource = new EventSource(
        `/api/projects/${projectId}/environments/${environment.id}/containers/${container.container_id}/metrics/stream`
      )

      eventSource.onopen = () => {
        setConnectedCount((c) => c + 1)
      }

      eventSource.onmessage = (event) => {
        try {
          const data: MetricsSnapshot = JSON.parse(event.data)
          metricsMapRef.current.set(container.container_id, data)
          // Immediately compute and surface live metrics
          const agg = aggregateMetrics()
          if (agg) {
            setLiveMetrics(agg)
            onMetricsUpdateRef.current?.(agg)
          }
          if (!hasDataRef.current) {
            hasDataRef.current = true
            setHasData(true)
          }
        } catch {
          // ignore parse errors
        }
      }

      eventSource.onerror = () => {
        setConnectedCount((c) => Math.max(0, c - 1))
        eventSource.close()
        eventSourcesRef.current.delete(container.container_id)
      }

      eventSourcesRef.current.set(container.container_id, eventSource)
    },
    [projectId, environment.id, aggregateMetrics]
  )

  useEffect(() => {
    for (const container of activeContainers) {
      connectToStream(container)
    }
    const sources = eventSourcesRef.current
    return () => {
      for (const es of sources.values()) {
        es.close()
      }
      sources.clear()
    }
  }, [activeContainers, connectToStream])

  // Push chart data points on a stable 2s interval
  useEffect(() => {
    if (!hasData) return

    const interval = setInterval(() => {
      const agg = aggregateMetrics()
      if (!agg) return

      setLiveMetrics(agg)
      onMetricsUpdateRef.current?.(agg)

      const perContainer = buildPerContainerPoint()

      const now = Date.now()
      setChartHistory((prev) => {
        const next = [
          ...prev,
          {
            time: format(now, 'HH:mm:ss'),
            timestamp: now,
            networkRx: agg.networkRxKBs,
            networkTx: agg.networkTxKBs,
            ...perContainer,
          },
        ]
        return next.length > MAX_DATA_POINTS
          ? next.slice(next.length - MAX_DATA_POINTS)
          : next
      })
    }, 2000)

    return () => clearInterval(interval)
  }, [hasData, aggregateMetrics, buildPerContainerPoint])

  const isConnected = connectedCount > 0

  if (activeContainers.length === 0) {
    return (
      <div className="flex items-center justify-center py-12 text-sm text-muted-foreground">
        No running containers
      </div>
    )
  }

  const chartsReady = chartHistory.length >= 2

  return (
    <div className="grid grid-cols-1 lg:grid-cols-2 gap-4">
      {/* CPU Line Chart */}
      <div>
        <div className="flex items-center justify-between mb-2">
          <div className="flex items-center gap-2">
            <p className="text-sm font-medium">CPU Usage</p>
            <span className="text-xs text-muted-foreground">
              {activeContainers.length}{' '}
              {activeContainers.length === 1 ? 'replica' : 'replicas'}
            </span>
          </div>
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            {isConnected && (
              <span className="flex items-center gap-1">
                <span className="h-1.5 w-1.5 rounded-full bg-green-500" />
                Live
              </span>
            )}
            {liveMetrics ? (
              <span>Total {liveMetrics.cpu.toFixed(1)}%</span>
            ) : (
              <span className="flex items-center gap-1">
                <span className="h-1.5 w-1.5 rounded-full bg-muted-foreground animate-pulse" />
                Connecting...
              </span>
            )}
          </div>
        </div>
        {chartsReady ? (
          <ChartContainer config={cpuChartConfig} className="h-[200px] w-full">
            <LineChart
              data={chartHistory}
              margin={{ left: 12, right: 12, top: 8, bottom: 0 }}
            >
              <CartesianGrid
                strokeDasharray="3 3"
                vertical={false}
                className="stroke-muted/30"
              />
              <XAxis
                dataKey="time"
                tickLine={false}
                axisLine={false}
                tickMargin={8}
                minTickGap={50}
                tick={{ fontSize: 11 }}
              />
              <YAxis
                tickLine={false}
                axisLine={false}
                tickMargin={8}
                tick={{ fontSize: 11 }}
                domain={[0, (max: number) => Math.max(max * 1.2, 10)]}
                tickFormatter={(v) => `${v}%`}
                width={45}
              />
              <ChartTooltip
                content={
                  <ChartTooltipContent
                    formatter={(value, name) => [
                      `${Number(value).toFixed(2)}%`,
                      cpuChartConfig[name as string]?.label ?? name,
                    ]}
                  />
                }
              />
              <ChartLegend content={<ChartLegendContent />} />
              {Array.from(containerSeries.values()).map((series) => (
                <Line
                  key={series.key}
                  dataKey={`cpu_${series.key}`}
                  name={`cpu_${series.key}`}
                  type="monotone"
                  stroke={`var(--color-cpu_${series.key})`}
                  strokeWidth={2}
                  dot={false}
                  activeDot={{ r: 4 }}
                />
              ))}
            </LineChart>
          </ChartContainer>
        ) : (
          <Skeleton className="h-[200px] w-full" />
        )}
      </div>

      {/* Memory Line Chart */}
      <div>
        <div className="flex items-center justify-between mb-2">
          <div className="flex items-center gap-2">
            <p className="text-sm font-medium">Memory Usage</p>
            <span className="text-xs text-muted-foreground">
              {activeContainers.length}{' '}
              {activeContainers.length === 1 ? 'replica' : 'replicas'}
            </span>
          </div>
          <span className="text-xs text-muted-foreground">
            {liveMetrics ? (
              <>
                Total {liveMetrics.memoryMb.toFixed(0)} MB (
                {liveMetrics.memoryPercent.toFixed(1)}%)
              </>
            ) : (
              <span className="flex items-center gap-1">
                <span className="h-1.5 w-1.5 rounded-full bg-muted-foreground animate-pulse" />
                Connecting...
              </span>
            )}
          </span>
        </div>
        {chartsReady ? (
          <ChartContainer
            config={memoryChartConfig}
            className="h-[200px] w-full"
          >
            <LineChart
              data={chartHistory}
              margin={{ left: 12, right: 12, top: 8, bottom: 0 }}
            >
              <CartesianGrid
                strokeDasharray="3 3"
                vertical={false}
                className="stroke-muted/30"
              />
              <XAxis
                dataKey="time"
                tickLine={false}
                axisLine={false}
                tickMargin={8}
                minTickGap={50}
                tick={{ fontSize: 11 }}
              />
              <YAxis
                tickLine={false}
                axisLine={false}
                tickMargin={8}
                tick={{ fontSize: 11 }}
                domain={[0, 'auto']}
                tickFormatter={(v) => `${v} MB`}
                width={55}
              />
              <ChartTooltip
                content={
                  <ChartTooltipContent
                    formatter={(value, name) => [
                      `${Number(value).toFixed(1)} MB`,
                      memoryChartConfig[name as string]?.label ?? name,
                    ]}
                  />
                }
              />
              <ChartLegend content={<ChartLegendContent />} />
              {Array.from(containerSeries.values()).map((series) => (
                <Line
                  key={series.key}
                  dataKey={`mem_${series.key}`}
                  name={`mem_${series.key}`}
                  type="monotone"
                  stroke={`var(--color-mem_${series.key})`}
                  strokeWidth={2}
                  dot={false}
                  activeDot={{ r: 4 }}
                />
              ))}
            </LineChart>
          </ChartContainer>
        ) : (
          <Skeleton className="h-[200px] w-full" />
        )}
      </div>

      {/* Network I/O Line Chart */}
      <div className="lg:col-span-2">
        <div className="flex items-center justify-between mb-2">
          <p className="text-sm font-medium">Network I/O</p>
          <span className="text-xs text-muted-foreground">
            {liveMetrics ? (
              <>
                <span className="inline-flex items-center gap-1">
                  <span
                    className="inline-block h-2 w-2 rounded-full"
                    style={{ background: 'var(--chart-3)' }}
                  />
                  {formatNetworkRate(liveMetrics.networkRxKBs)} in
                </span>
                <span className="mx-2">/</span>
                <span className="inline-flex items-center gap-1">
                  <span
                    className="inline-block h-2 w-2 rounded-full"
                    style={{ background: 'var(--chart-5)' }}
                  />
                  {formatNetworkRate(liveMetrics.networkTxKBs)} out
                </span>
              </>
            ) : (
              <span className="flex items-center gap-1">
                <span className="h-1.5 w-1.5 rounded-full bg-muted-foreground animate-pulse" />
                Connecting...
              </span>
            )}
          </span>
        </div>
        {chartsReady ? (
          <ChartContainer
            config={networkChartConfig}
            className="h-[200px] w-full"
          >
            <LineChart
              data={chartHistory}
              margin={{ left: 12, right: 12, top: 8, bottom: 0 }}
            >
              <CartesianGrid
                strokeDasharray="3 3"
                vertical={false}
                className="stroke-muted/30"
              />
              <XAxis
                dataKey="time"
                tickLine={false}
                axisLine={false}
                tickMargin={8}
                minTickGap={50}
                tick={{ fontSize: 11 }}
              />
              <YAxis
                tickLine={false}
                axisLine={false}
                tickMargin={8}
                tick={{ fontSize: 11 }}
                domain={[0, 'auto']}
                tickFormatter={(v) => formatNetworkRate(v)}
                width={70}
              />
              <ChartTooltip
                content={
                  <ChartTooltipContent
                    formatter={(value, name) => [
                      formatNetworkRate(Number(value)),
                      name === 'networkRx' ? 'Received' : 'Sent',
                    ]}
                  />
                }
              />
              <ChartLegend content={<ChartLegendContent />} />
              <Line
                dataKey="networkRx"
                type="monotone"
                stroke="var(--color-networkRx)"
                strokeWidth={2}
                dot={false}
                activeDot={{ r: 4 }}
              />
              <Line
                dataKey="networkTx"
                type="monotone"
                stroke="var(--color-networkTx)"
                strokeWidth={2}
                dot={false}
                activeDot={{ r: 4 }}
              />
            </LineChart>
          </ChartContainer>
        ) : (
          <Skeleton className="h-[200px] w-full" />
        )}
      </div>
    </div>
  )
}
