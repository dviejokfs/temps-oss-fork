import { client } from '@/api/client/client.gen'
import type { PluginManifest } from '@/types/plugins'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'

export const PLUGINS_QUERY_KEY = ['external-plugins']

/**
 * Fetch the list of external plugin manifests from /api/x/plugins.
 * Returns an empty array if the endpoint is unavailable (e.g., no plugins loaded).
 */
async function fetchPluginManifests(): Promise<PluginManifest[]> {
  try {
    const response = await client.get<PluginManifest[]>({
      url: '/x/plugins',
    })
    return response.data ?? []
  } catch {
    // Endpoint may not exist if no external plugins are configured.
    // Degrade gracefully — no plugins is the default.
    return []
  }
}

/** Response from POST /x/plugins/reload */
export interface ReloadPluginsResponse {
  loaded: number
  plugins: string[]
  message: string
}

/**
 * React Query hook to get the list of external plugins.
 * Caches for 5 minutes since plugins rarely change at runtime.
 * Never throws — returns an empty list on failure.
 */
export function usePlugins() {
  return useQuery({
    queryKey: PLUGINS_QUERY_KEY,
    queryFn: fetchPluginManifests,
    staleTime: 5 * 60 * 1000,
    gcTime: 10 * 60 * 1000,
    retry: false,
  })
}

/**
 * Mutation hook to reload all external plugins.
 * On success, invalidates the plugins query so the UI refreshes.
 */
export function useReloadPlugins() {
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: async (): Promise<ReloadPluginsResponse> => {
      const response = await client.post<ReloadPluginsResponse>({
        url: '/x/plugins/reload',
      })
      return response.data!
    },
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: PLUGINS_QUERY_KEY })
    },
  })
}

// ---------------------------------------------------------------------------
// Plugin marketplace (install-from-registry) — mirrors the handlers in
// crates/temps-external-plugins/src/handler.rs. These three endpoints are
// not in the generated OpenAPI client because that client is regenerated
// from the live server and hasn't been refreshed for this feature yet, so
// calls go through the same raw `client.get/post` escape hatch used above
// for `/x/plugins` and `/x/plugins/reload`. Swap for the generated SDK
// functions once `bun run openapi-ts` picks these up.

/** A single platform's download descriptor inside a registry manifest. */
export interface PluginPlatformAsset {
  url: string
  sha256: string
}

/** Registry manifest for an installable plugin (from the release host). */
export interface PluginRegistryManifest {
  name: string
  version: string
  platforms: Record<string, PluginPlatformAsset>
}

/** Response from GET /x/plugins/available */
export interface PluginAvailabilityResponse {
  /** Whether the plugin binary is already installed (present on disk). */
  installed: boolean
  /** The manifest fetched from the registry, if reachable. */
  manifest?: PluginRegistryManifest | null
  /** Human-readable reason when the manifest could not be fetched. */
  reason?: string | null
}

/** Response from GET /x/plugins/{name}/status */
export interface PluginStatusResponse {
  /** Whether the plugin is installed and its process is running. */
  configured: boolean
  /** Why the plugin is not configured (when `configured` is false). */
  reason?: string | null
  /** Console path the operator should visit to configure or install it. */
  setup_path?: string | null
}

/** Response from POST /x/plugins/install */
export interface InstallPluginResponse {
  name: string
  version: string
  path: string
  reloaded: boolean
  message: string
}

export const PLUGIN_AVAILABILITY_QUERY_KEY = (name: string) => [
  'external-plugins',
  'available',
  name,
]

export const PLUGIN_STATUS_QUERY_KEY = (name: string) => [
  'external-plugins',
  'status',
  name,
]

/**
 * React Query hook for GET /x/plugins/available.
 *
 * SystemAdmin-gated on the backend: a non-admin viewer of the Plugins page
 * gets a 403 here, which the caller should render as-is rather than retry.
 */
export function usePluginAvailability(name: string) {
  return useQuery({
    queryKey: PLUGIN_AVAILABILITY_QUERY_KEY(name),
    queryFn: async (): Promise<PluginAvailabilityResponse> => {
      const response = await client.get<PluginAvailabilityResponse, unknown, true>({
        url: '/x/plugins/available',
        throwOnError: true,
      })
      return response.data
    },
    staleTime: 60 * 1000,
    retry: false,
  })
}

/**
 * React Query hook for GET /x/plugins/{name}/status.
 * Any authenticated user may call this — it is the capability-check
 * endpoint that drives the onboarding state in the UI.
 */
export function usePluginStatus(name: string) {
  return useQuery({
    queryKey: PLUGIN_STATUS_QUERY_KEY(name),
    queryFn: async (): Promise<PluginStatusResponse> => {
      const response = await client.get<PluginStatusResponse, unknown, true>({
        url: '/x/plugins/{name}/status',
        path: { name },
        throwOnError: true,
      })
      return response.data
    },
    staleTime: 30 * 1000,
    retry: false,
  })
}

/**
 * Mutation hook for POST /x/plugins/install.
 * Invalidates the availability, status, and installed-plugins queries
 * regardless of outcome — a failed install can still have partially
 * changed on-disk state (e.g. binary written, process start failed), so
 * the queries need to re-read the server's view either way.
 */
export function useInstallPlugin(name: string) {
  const queryClient = useQueryClient()

  return useMutation({
    mutationFn: async (version?: string): Promise<InstallPluginResponse> => {
      const response = await client.post<InstallPluginResponse, unknown, true>({
        url: '/x/plugins/install',
        body: { name, version },
        throwOnError: true,
      })
      return response.data
    },
    onSettled: () => {
      queryClient.invalidateQueries({ queryKey: PLUGIN_AVAILABILITY_QUERY_KEY(name) })
      queryClient.invalidateQueries({ queryKey: PLUGIN_STATUS_QUERY_KEY(name) })
      queryClient.invalidateQueries({ queryKey: PLUGINS_QUERY_KEY })
    },
  })
}
