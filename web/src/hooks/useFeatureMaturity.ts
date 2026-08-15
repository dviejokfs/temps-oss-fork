import { useQuery } from '@tanstack/react-query'
import type { FeatureMaturity } from '@/lib/feature-maturity'

const QUERY_KEY = ['platform', 'feature-maturity'] as const

async function fetchFeatureMaturity(): Promise<FeatureMaturity[]> {
  const response = await fetch('/api/v1/platform/feature-maturity', {
    credentials: 'same-origin',
    headers: { Accept: 'application/json' },
  })
  if (!response.ok) {
    throw new Error(
      `Unable to load feature maturity metadata (${response.status})`
    )
  }
  return (await response.json()) as FeatureMaturity[]
}

export function useFeatureMaturity(featureKey?: string) {
  const query = useQuery({
    queryKey: QUERY_KEY,
    queryFn: fetchFeatureMaturity,
    staleTime: Infinity,
    retry: false,
  })

  return {
    ...query,
    feature: featureKey
      ? query.data?.find((entry) => entry.key === featureKey)
      : undefined,
  }
}
