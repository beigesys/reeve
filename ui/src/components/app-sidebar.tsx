import { Link, useMatchRoute } from '@tanstack/react-router'
import {
  Activity,
  History,
  KeyRound,
  MonitorSmartphone,
  Network,
  Package,
  Rocket,
  Ticket,
  Upload,
} from 'lucide-react'
import {
  Sidebar,
  SidebarContent,
  SidebarGroup,
  SidebarGroupContent,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
} from '@/components/ui/sidebar'

const NAV = [
  { to: '/fleet', label: 'Fleet', icon: Network },
  { to: '/devices', label: 'Devices', icon: MonitorSmartphone },
  { to: '/deploy', label: 'Deploy', icon: Upload },
  { to: '/packages', label: 'Packages', icon: Package },
  { to: '/rollouts', label: 'Rollouts', icon: Rocket },
  { to: '/history', label: 'History', icon: History },
  { to: '/secrets', label: 'Secrets', icon: KeyRound },
  { to: '/enrollment', label: 'Enrollment', icon: Ticket },
  { to: '/ops', label: 'Ops', icon: Activity },
] as const

export function AppSidebar() {
  const matchRoute = useMatchRoute()
  return (
    <Sidebar collapsible="icon">
      <SidebarHeader>
        <div className="flex h-8 items-center px-2">
          <Link
            to="/fleet"
            className="text-lg font-semibold tracking-tight group-data-[collapsible=icon]:hidden"
          >
            reeve
          </Link>
        </div>
      </SidebarHeader>
      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupContent>
            <SidebarMenu>
              {NAV.map(({ to, label, icon: Icon }) => (
                <SidebarMenuItem key={to}>
                  <SidebarMenuButton
                    asChild
                    tooltip={label}
                    isActive={!!matchRoute({ to, fuzzy: true })}
                  >
                    <Link to={to}>
                      <Icon />
                      <span>{label}</span>
                    </Link>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroupContent>
        </SidebarGroup>
      </SidebarContent>
    </Sidebar>
  )
}
