import { useState } from 'react'
import { CircleAlert, FileText } from 'lucide-react'
import type { DeviceComponentState, DeviceDeploymentState } from '@/api/model'
import { Button } from '@/components/ui/button'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from '@/components/ui/dialog'
import { DeployLogsViewer } from '@/components/deploy-logs-viewer'
import { DeploymentStateBadge } from '@/components/deployment-state-badge'
import { cn } from '@/lib/utils'
import { fmtRfc3339, fmtUnix } from '@/lib/format'

/** Per-component state row: name, state pill, and its own error. */
function ComponentRow({ component }: { component: DeviceComponentState }) {
  return (
    <div className="flex flex-col gap-1 border-b px-3 py-2 last:border-b-0">
      <div className="flex min-w-0 flex-wrap items-center gap-2">
        <span className="min-w-0 font-mono text-xs break-all">
          {component.name}
        </span>
        <DeploymentStateBadge state={component.state} />
      </div>
      {component.error && (
        <p className="text-xs break-words text-red-600 dark:text-red-400">
          {component.error}
        </p>
      )}
    </div>
  )
}

function DetailBody({
  deviceId,
  deployment,
  open,
}: {
  deviceId: string
  deployment: DeviceDeploymentState
  open: boolean
}) {
  const components = deployment.components ?? []
  const failed = deployment.state === 'failed'
  return (
    <div className="flex min-h-0 min-w-0 flex-1 flex-col gap-4 overflow-y-auto">
      <div className="flex flex-col gap-2">
        <div className="flex flex-wrap items-center gap-2">
          <DeploymentStateBadge state={deployment.state} />
          <span className="text-xs text-muted-foreground">
            reported {fmtUnix(deployment.receivedAt)}
          </span>
          {deployment.observedAt && (
            <span className="text-xs text-muted-foreground">
              · observed {fmtRfc3339(deployment.observedAt)}
            </span>
          )}
        </div>
        <p className="font-mono text-xs break-all text-muted-foreground">
          {deployment.deploymentId}
        </p>
      </div>

      {failed && deployment.error && (
        <Alert variant="destructive">
          <CircleAlert />
          <AlertTitle>Deployment failed</AlertTitle>
          <AlertDescription>
            <span className="break-words whitespace-pre-wrap">
              {deployment.error}
            </span>
          </AlertDescription>
        </Alert>
      )}

      <section className="flex flex-col gap-2">
        <h3 className="text-sm font-medium">Components</h3>
        {components.length === 0 ? (
          <p className="text-sm text-muted-foreground">
            No per-component detail was reported.
          </p>
        ) : (
          <div className="min-w-0 rounded-md border">
            {components.map((c) => (
              <ComponentRow key={c.name} component={c} />
            ))}
          </div>
        )}
      </section>

      <section className="flex min-h-64 flex-col gap-2">
        <h3 className="text-sm font-medium">Deploy logs</h3>
        <p className="text-xs text-muted-foreground">
          Captured <span className="font-mono">docker compose</span> output,
          newest first.
        </p>
        <DeployLogsViewer
          deviceId={deviceId}
          deploymentId={deployment.deploymentId}
          enabled={open}
        />
      </section>
    </div>
  )
}

/**
 * Read-only detail for one deployment on a device: overall state, the
 * failure reason (when failed), per-component states (which component
 * failed and why), and the captured compose logs. All long text wraps
 * or scrolls inside its own pane — the page never scrolls sideways.
 */
export function DeploymentDetailDialog({
  deviceId,
  deployment,
  className,
}: {
  deviceId: string
  deployment: DeviceDeploymentState
  className?: string
}) {
  const [open, setOpen] = useState(false)
  return (
    <Dialog open={open} onOpenChange={setOpen}>
      <DialogTrigger asChild>
        <Button variant="outline" size="sm" className={cn(className)}>
          <FileText className="size-4" />
          Details
        </Button>
      </DialogTrigger>
      <DialogContent className="flex max-h-[85vh] flex-col gap-4 sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>Deployment</DialogTitle>
          <DialogDescription>
            Read-only detail as last reported by this device.
          </DialogDescription>
        </DialogHeader>
        <DetailBody deviceId={deviceId} deployment={deployment} open={open} />
      </DialogContent>
    </Dialog>
  )
}
