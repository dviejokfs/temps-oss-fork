// SPDX-FileCopyrightText: 2024-2026 Temps Contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

import { ProjectResponse } from '@/api/client'
import { getProjectsOptions } from '@/api/client/@tanstack/react-query.gen'
import { useQuery } from '@tanstack/react-query'
import { createContext, useContext } from 'react'
import { useAuth } from '@/contexts/AuthContext'

interface ProjectsContextType {
  projects: ProjectResponse[]
  isLoading: boolean
}

const ProjectsContext = createContext<ProjectsContextType>({
  projects: [],
  isLoading: false,
})

export function ProjectsProvider({ children }: { children: React.ReactNode }) {
  const { user } = useAuth()
  const { data, isLoading } = useQuery({
    ...getProjectsOptions({
      query: {
        page: 1,
        per_page: 4,
      },
    }),
    // ProjectsProvider wraps the whole app, including the logged-out login
    // screen. Without this, every logged-out visitor's browser fires this
    // authenticated query, gets a 401, and (via App.tsx's QueryCache.onError)
    // invalidates the current-user query -- which flips AuthContext's
    // loading state and remounts <Login />, wiping in-progress form input.
    enabled: !!user,
  })

  return (
    <ProjectsContext.Provider
      value={{ projects: data?.projects || [], isLoading }}
    >
      {children}
    </ProjectsContext.Provider>
  )
}

export function useProjects() {
  return useContext(ProjectsContext)
}
