import { api } from './client'
import type { 
  ApiKeyListResponse, 
  AddApiKeyRequest, 
  UpdateApiKeyRequest, 
  SuccessResponse 
} from '@/types/api'

export async function getApiKeys(): Promise<ApiKeyListResponse> {
  const { data } = await api.get<ApiKeyListResponse>('/api-keys')
  return data
}

export async function addApiKey(req: AddApiKeyRequest): Promise<{ success: boolean; message: string; apiKeyId: number; key: string }> {
  const { data } = await api.post('/api-keys', req)
  return data
}

export async function updateApiKey(id: number, req: UpdateApiKeyRequest): Promise<SuccessResponse> {
  const { data } = await api.put<SuccessResponse>(`/api-keys/${id}`, req)
  return data
}

export async function deleteApiKey(id: number): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/api-keys/${id}`)
  return data
}

export async function getPools(): Promise<string[]> {
  const { data } = await api.get<string[]>('/pools')
  return data
}
