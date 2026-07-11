import { Link, createFileRoute } from '@tanstack/react-router'
import { ArrowLeft } from 'lucide-react'
import { useMe } from '@/api/endpoints/auth/auth'
import { useDetail } from '@/api/endpoints/devices/devices'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@/components/ui/card'
import { DeviceForm } from '@/components/device-form'

export const Route = createFileRoute('/_app/devices/$device-id/edit')({
  component: DeviceEditPage,
})

function DeviceEditPage() {
  const params = Route.useParams()
  const deviceId = params['device-id']
  const detail = useDetail(deviceId)
  const me = useMe()

  const device = detail.data?.status === 200 ? detail.data.data : undefined
  const role = me.data?.status === 200 ? me.data.data.effectiveRole : undefined
  const operator = role === 'admin' || role === 'operator'

  return (
    <div className="flex flex-col gap-4 p-6">
      <div className="flex items-center gap-3">
        <Button variant="ghost" size="sm" asChild>
          <Link to="/devices/$device-id" params={{ 'device-id': deviceId }}>
            <ArrowLeft className="size-4" />
            Back
          </Link>
        </Button>
        <h1 className="text-xl font-semibold tracking-tight">
          Manage {device?.displayName ?? device?.hostname ?? deviceId}
        </h1>
      </div>

      <Card className="max-w-3xl">
        <CardHeader>
          <CardTitle className="text-base">Device settings</CardTitle>
        </CardHeader>
        <CardContent>
          {detail.data && detail.data.status === 404 ? (
            <p className="text-sm text-destructive">Unknown device.</p>
          ) : !operator ? (
            <p className="text-sm text-muted-foreground">
              You need operator access to manage a device.
            </p>
          ) : !device ? (
            <p className="text-sm text-muted-foreground">Loading…</p>
          ) : (
            <DeviceForm key={device.deviceId} device={device} />
          )}
        </CardContent>
      </Card>
    </div>
  )
}
