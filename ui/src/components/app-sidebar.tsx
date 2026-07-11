import { Link } from '@tanstack/react-router'
import {
  Activity,
  GitBranch,
  KeyRound,
  MonitorSmartphone,
  Package,
  Rocket,
  Ticket,
} from 'lucide-react'
import { cn } from '@/lib/utils'

const NAV = [
  { to: '/devices', label: 'Devices', icon: MonitorSmartphone },
  { to: '/tree', label: 'Tree', icon: GitBranch },
  { to: '/packages', label: 'Packages', icon: Package },
  { to: '/rollouts', label: 'Rollouts', icon: Rocket },
  { to: '/secrets', label: 'Secrets', icon: KeyRound },
  { to: '/enrollment', label: 'Enrollment', icon: Ticket },
  { to: '/ops', label: 'Ops', icon: Activity },
] as const

export function AppSidebar() {
  return (
    <aside className="flex w-52 shrink-0 flex-col border-r bg-sidebar text-sidebar-foreground">
      <div className="flex h-14 items-center border-b px-4">
        <Link to="/devices" className="text-lg font-semibold tracking-tight">
          reeve
        </Link>
      </div>
      <nav className="flex flex-col gap-1 p-2">
        {NAV.map(({ to, label, icon: Icon }) => (
          <Link
            key={to}
            to={to}
            className={cn(
              'flex items-center gap-2.5 rounded-md px-3 py-2 text-sm',
              'text-sidebar-foreground/70 hover:bg-sidebar-accent hover:text-sidebar-accent-foreground',
            )}
            activeProps={{
              className: 'bg-sidebar-accent text-sidebar-accent-foreground font-medium',
            }}
          >
            <Icon className="size-4" />
            {label}
          </Link>
        ))}
      </nav>
    </aside>
  )
}
