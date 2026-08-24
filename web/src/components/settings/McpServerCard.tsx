import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { CopyButton } from '@/components/ui/copy-button'
import { Label } from '@/components/ui/label'
import { Skeleton } from '@/components/ui/skeleton'
import { Switch } from '@/components/ui/switch'
import { useSettings, useUpdateSettings } from '@/hooks/useSettings'
import { AlertTriangle, Bot, ExternalLink } from 'lucide-react'
import { toast } from 'sonner'

/**
 * MCP endpoints are mounted at the server root (not under /api), same as the
 * CLI's mcp command family -- see apps/temps-cli/src/commands/mcp/index.ts.
 * external_url is the operator-declared public address; fall back to the
 * origin this page itself was loaded from for local/dev instances that never
 * set it.
 */
function mcpBaseUrl(externalUrl: string | null | undefined): string {
  return externalUrl || window.location.origin
}

export function McpServerCard() {
  const { data: settings, isLoading, error } = useSettings()
  const updateSettings = useUpdateSettings()

  if (isLoading) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Bot className="h-5 w-5" />
            MCP Server
          </CardTitle>
        </CardHeader>
        <CardContent>
          <Skeleton className="h-16 w-full" />
        </CardContent>
      </Card>
    )
  }

  if (error || !settings) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Bot className="h-5 w-5" />
            MCP Server
          </CardTitle>
        </CardHeader>
        <CardContent>
          <Alert variant="destructive">
            <AlertTriangle className="h-4 w-4" />
            <AlertTitle>Failed to load settings</AlertTitle>
            <AlertDescription>
              The server returned an error. Check console logs or contact your
              administrator.
            </AlertDescription>
          </Alert>
        </CardContent>
      </Card>
    )
  }

  const enabled = settings.mcp_server?.enabled ?? false
  const baseUrl = mcpBaseUrl(settings.external_url)

  const onCheckedChange = (checked: boolean) => {
    updateSettings.mutate(
      { mcp_server: { enabled: checked } },
      {
        onSuccess: () =>
          toast.success(
            checked ? 'MCP server enabled' : 'MCP server disabled'
          ),
      }
    )
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Bot className="h-5 w-5" />
          MCP Server
        </CardTitle>
        <CardDescription>
          Lets AI clients (Claude Code, Claude Desktop, Codex, Cursor, VS
          Code, Windsurf, Zed) connect to this Temps instance over the Model
          Context Protocol — e.g. ask "list my Temps projects" or "deploy the
          latest commit" and have the assistant call real tools against this
          instance. Every write action still requires a separate confirmation
          before it executes.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex items-start justify-between rounded-lg border p-3">
          <div className="space-y-0.5">
            <Label htmlFor="mcp-server-enabled" className="text-sm">
              Enable MCP server
            </Label>
            <p className="text-xs text-muted-foreground max-w-prose">
              Off by default — a fresh install never exposes the MCP endpoint
              until an admin turns it on here.
            </p>
          </div>
          <Switch
            id="mcp-server-enabled"
            checked={enabled}
            disabled={updateSettings.isPending}
            onCheckedChange={onCheckedChange}
          />
        </div>

        {enabled ? (
          <div className="space-y-3 rounded-lg border p-3">
            <div className="space-y-1">
              <Label className="text-sm">MCP endpoint</Label>
              <div className="flex items-center gap-2">
                <code className="flex-1 truncate rounded bg-muted px-2 py-1 text-xs">
                  {baseUrl}/mcp
                </code>
                <CopyButton
                  value={`${baseUrl}/mcp`}
                  minimal
                  label="Copy MCP endpoint"
                  className="shrink-0"
                />
              </div>
            </div>
            <div className="space-y-1">
              <Label className="text-sm">Connect a client</Label>
              <div className="flex items-center gap-2">
                <code className="flex-1 truncate rounded bg-muted px-2 py-1 text-xs">
                  bunx @temps-sdk/cli mcp add
                </code>
                <CopyButton
                  value="bunx @temps-sdk/cli mcp add"
                  minimal
                  label="Copy command"
                  className="shrink-0"
                />
              </div>
              <p className="text-xs text-muted-foreground">
                Runs an installer wizard that mints a scoped API key and
                writes the right config for whichever AI client you pick.
              </p>
            </div>
            <a
              href="https://temps.sh/docs/set-up-mcp-locally"
              target="_blank"
              rel="noreferrer"
              className="inline-flex items-center gap-1 text-xs text-primary hover:underline"
            >
              Full setup guide
              <ExternalLink className="h-3 w-3" />
            </a>
          </div>
        ) : (
          <Alert>
            <Bot className="h-4 w-4" />
            <AlertTitle>MCP is off</AlertTitle>
            <AlertDescription>
              Turn it on above, then run{' '}
              <code className="px-1 py-0.5 bg-muted rounded text-xs">
                bunx @temps-sdk/cli mcp add
              </code>{' '}
              to connect an AI client.
            </AlertDescription>
          </Alert>
        )}
      </CardContent>
    </Card>
  )
}
