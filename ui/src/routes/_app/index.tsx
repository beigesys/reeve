import { createFileRoute, redirect } from '@tanstack/react-router'

export const Route = createFileRoute('/_app/')({
  // The fleet is the home page.
  beforeLoad: () => {
    throw redirect({ to: '/devices' })
  },
})
