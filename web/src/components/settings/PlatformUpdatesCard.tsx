import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { CopyButton } from '@/components/ui/copy-button'
import { Label } from '@/components/ui/label'
import { Switch } from '@/components/ui/switch'
import { useSelfUpdateCapability } from '@/hooks/useSelfUpdate'
import { useSettings, useUpdateSettings } from '@/hooks/useSettings'
import { AlertTriangle, Info, Terminal } from 'lucide-react'
import { toast } from 'sonner'

/**
 * Operator control for the console's "Update now" action.
 *
 * Shows the toggle *and* what the server independently makes of it — an admin
 * who turns this on while the host has no supervisor (or is a container) would
 * otherwise be left wondering why the button never appears.
 */
export function PlatformUpdatesCard() {
  const { data: settings } = useSettings()
  const updateSettings = useUpdateSettings()
  const { data: capability } = useSelfUpdateCapability()

  const enabled = settings?.self_update?.enabled ?? true
  // A launch flag beats the database, so the toggle is inert (and says so)
  // rather than pretending to control something it doesn't.
  const overriddenByFlag = capability?.blocker === 'disabled_by_flag'

  const handleToggle = async (next: boolean) => {
    try {
      await updateSettings.mutateAsync({
        self_update: { enabled: next },
      })
      toast.success(
        next
          ? 'Updates can now be applied from the console'
          : 'Console updates disabled'
      )
    } catch {
      // useUpdateSettings surfaces the failure toast.
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle>Platform updates</CardTitle>
        <CardDescription>
          Whether admins can install a new temps release and restart the server
          from the console. The update banner and the manual command are shown
          either way.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="flex items-center justify-between gap-4">
          <div className="space-y-1">
            <Label htmlFor="self-update-enabled">
              Allow updates from the console
            </Label>
            <p className="text-sm text-muted-foreground">
              Requires the <span className="font-mono">platform:update</span>{' '}
              permission. Every attempt is audited.
            </p>
          </div>
          <Switch
            id="self-update-enabled"
            checked={enabled && !overriddenByFlag}
            disabled={overriddenByFlag || updateSettings.isPending}
            onCheckedChange={handleToggle}
          />
        </div>

        {overriddenByFlag && (
          <p className="flex gap-2 rounded border border-amber-300 bg-amber-50 p-2 text-sm text-amber-900 dark:border-amber-900/50 dark:bg-amber-950/30 dark:text-amber-200">
            <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0" />
            <span>
              This server was started with{' '}
              <span className="font-mono">--disable-self-update</span>, which
              overrides this setting. Remove the flag and restart temps to allow
              console updates.
            </span>
          </p>
        )}

        {capability && !capability.can_apply && !overriddenByFlag && (
          <div className="space-y-2 rounded border bg-muted/40 p-2 text-sm">
            <p className="flex gap-2">
              <Info className="mt-0.5 h-4 w-4 shrink-0 text-muted-foreground" />
              <span>{capability.reason}</span>
            </p>
            <div className="flex items-center gap-2">
              <Terminal className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
              <code className="min-w-0 flex-1 truncate font-mono text-xs">
                {capability.manual_command}
              </code>
              <CopyButton value={capability.manual_command} minimal />
            </div>
          </div>
        )}

        {capability?.can_apply && (
          <p className="text-xs text-muted-foreground">
            Managed by{' '}
            <span className="font-mono">{capability.supervisor}</span> ·
            replaces <span className="font-mono">{capability.binary_path}</span>
          </p>
        )}
      </CardContent>
    </Card>
  )
}
