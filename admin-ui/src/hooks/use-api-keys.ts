import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getApiKeys,
  addApiKey,
  updateApiKey,
  deleteApiKey,
  getPools,
} from '@/api/api-keys'
import type { AddApiKeyRequest, UpdateApiKeyRequest } from '@/types/api'

export function useApiKeys() {
  return useQuery({
    queryKey: ['api-keys'],
    queryFn: getApiKeys,
    refetchInterval: 30000,
  })
}

export function usePools() {
  return useQuery({
    queryKey: ['pools'],
    queryFn: getPools,
    refetchInterval: 60000,
  })
}

export function useAddApiKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: AddApiKeyRequest) => addApiKey(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['api-keys'] })
      queryClient.invalidateQueries({ queryKey: ['pools'] })
    },
  })
}

export function useUpdateApiKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateApiKeyRequest }) => updateApiKey(id, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['api-keys'] })
      queryClient.invalidateQueries({ queryKey: ['pools'] })
    },
  })
}

export function useDeleteApiKey() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteApiKey(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['api-keys'] })
      queryClient.invalidateQueries({ queryKey: ['pools'] })
    },
  })
}
