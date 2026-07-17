// 凭据状态响应
export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  name: string
  priority: number
  disabled: boolean
  failureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  hasProfileArn: boolean
  profileArn?: string | null
  importedAt?: string | null
  email?: string
  userName?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  endpoint: string
  pools: string[]
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
}

// 模型目录（per-credential）
export interface TokenLimits {
  maxInputTokens?: number | null
  maxOutputTokens?: number | null
}

export interface PromptCaching {
  maximumCacheCheckpointsPerRequest?: number | null
  minimumTokensPerCacheCheckpoint?: number | null
  supportsPromptCaching?: boolean | null
}

export interface KiroModel {
  modelId: string
  modelName: string
  description?: string | null
  rateMultiplier?: number | null
  rateUnit?: string | null
  supportedInputTypes?: string[] | null
  tokenLimits?: TokenLimits | null
  promptCaching?: PromptCaching | null
  additionalModelRequestFieldsSchema?: Record<string, unknown> | null
}

export interface CredentialCatalogResponse {
  credentialId: number
  /** cache = 内存缓存；upstream = 刚向上游拉取 */
  source: 'cache' | 'upstream' | string
  defaultModel?: { modelId: string } | null
  models: KiroModel[]
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

export type LoadBalancingMode = 'round_robin' | 'priority' | 'balanced'

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

export interface SetNameRequest {
  name: string
}

// 添加凭据请求
export interface AddCredentialRequest {
  name?: string
  /** Grok `/grok/admin` 使用的 xAI API token / OAuth access token。 */
  accessToken?: string
  refreshToken?: string
  authMethod?: 'social' | 'idc' | 'api_key' | 'token' | 'oauth'
  clientId?: string
  clientSecret?: string
  priority?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  email?: string
  userName?: string
  profileArn?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  kiroApiKey?: string
  endpoint?: string
  pools?: string[]
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  name: string
  email?: string
  userName?: string
  profileArn?: string
  importedAt?: string
}

export type GrokOAuthStatus = 'pending' | 'completed' | 'failed' | 'cancelled'

export interface GrokOAuthStartResponse {
  state: string
  authorizationUrl: string
  callbackUrl: string
  expiresInSeconds: number
}

export interface GrokOAuthStatusResponse {
  state: string
  status: GrokOAuthStatus
  authorizationUrl: string
  createdAt: string
  credentialId?: number
  email?: string
  error?: string
}

export interface ApiKeyEntry {
  id: number
  name: string
  key: string
  pools: string[]
  disabled: boolean
  createdAt?: string
}

export interface AddApiKeyRequest {
  name: string
  pools?: string[]
  key?: string
}

export interface UpdateApiKeyRequest {
  name?: string
  pools?: string[]
  disabled?: boolean
}

export interface ApiKeyListResponse {
  keys: ApiKeyEntry[]
}
