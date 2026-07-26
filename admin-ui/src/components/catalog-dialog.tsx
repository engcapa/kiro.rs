import { useMemo, useState } from 'react'
import { useQueryClient } from '@tanstack/react-query'
import {
  BookOpen,
  Brain,
  ChevronDown,
  ChevronRight,
  Copy,
  Loader2,
  RefreshCw,
  Search,
} from 'lucide-react'
import { toast } from 'sonner'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { getCredentialCatalog } from '@/api/credentials'
import { useCredentialCatalog } from '@/hooks/use-credentials'
import { parseError } from '@/lib/utils'
import type { KiroModel } from '@/types/api'

interface CatalogDialogProps {
  credentialId: number | null
  credentialLabel?: string
  open: boolean
  onOpenChange: (open: boolean) => void
}

function formatTokens(n?: number | null): string {
  if (n == null) return '-'
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(n % 1_000_000 === 0 ? 0 : 1)}M`
  if (n >= 1_000) return `${(n / 1_000).toFixed(n % 1_000 === 0 ? 0 : 1)}K`
  return String(n)
}

function schemaProperties(model: KiroModel): Record<string, unknown> | null {
  const schema = model.additionalModelRequestFieldsSchema
  if (!schema || typeof schema !== 'object') return null
  const props = (schema as { properties?: unknown }).properties
  if (!props || typeof props !== 'object') return null
  return props as Record<string, unknown>
}

function modelSupportsThinking(model: KiroModel): boolean {
  if (model.supportsReasoningEffort != null) return model.supportsReasoningEffort
  const props = schemaProperties(model)
  // thinking：Claude 家族；reasoning：gpt-5.x 系列；output_config：Claude effort
  return Boolean(
    props && ('thinking' in props || 'reasoning' in props || 'output_config' in props)
  )
}

/** 从形如 `{ properties: { effort: { enum: [...] } } }` 的容器中取 effort 枚举 */
function effortEnumFrom(container: unknown): string[] | null {
  if (!container || typeof container !== 'object') return null
  const nested = (container as { properties?: unknown }).properties
  const effort = (nested as { effort?: { enum?: unknown } } | undefined)?.effort
  if (!effort || !Array.isArray(effort.enum)) return null
  return effort.enum.filter((v): v is string => typeof v === 'string')
}

function extractEffortLevels(model: KiroModel): string[] {
  if (model.reasoningEfforts && model.reasoningEfforts.length > 0) {
    return model.reasoningEfforts.map((option) => option.value)
  }
  const props = schemaProperties(model)
  if (!props) return []
  // 兼容三种形状：顶层 effort、output_config.effort（Claude）、reasoning.effort（gpt-5.x）
  const topEffort = props.effort as { enum?: unknown } | undefined
  if (topEffort && Array.isArray(topEffort.enum)) {
    return topEffort.enum.filter((v): v is string => typeof v === 'string')
  }
  return effortEnumFrom(props.reasoning) ?? effortEnumFrom(props.output_config) ?? []
}

/** 从 schema 的 effort 属性中取 default（reasoning.effort / output_config.effort） */
function defaultEffortFrom(container: unknown): string | null {
  if (!container || typeof container !== 'object') return null
  const nested = (container as { properties?: unknown }).properties
  const effort = (nested as { effort?: { default?: unknown } } | undefined)?.effort
  return typeof effort?.default === 'string' ? effort.default : null
}

function extractDefaultEffort(model: KiroModel): string | null {
  if (model.defaultReasoningEffort) return model.defaultReasoningEffort
  const props = schemaProperties(model)
  if (!props) return null
  return defaultEffortFrom(props.reasoning) ?? defaultEffortFrom(props.output_config)
}

function ModelCard({ model, defaultModelId }: { model: KiroModel; defaultModelId?: string }) {
  const [expanded, setExpanded] = useState(false)
  const thinking = modelSupportsThinking(model)
  const efforts = extractEffortLevels(model)
  const defaultEffort = extractDefaultEffort(model)
  const isDefault = defaultModelId === model.modelId
  const schema = model.additionalModelRequestFieldsSchema
  const fieldKeys = schemaProperties(model) ? Object.keys(schemaProperties(model)!) : []

  return (
    <div className="rounded-lg border bg-card text-card-foreground shadow-sm">
      <button
        type="button"
        className="flex w-full items-start gap-3 p-3 text-left hover:bg-muted/40 transition-colors"
        onClick={() => setExpanded((v) => !v)}
      >
        <span className="mt-0.5 text-muted-foreground shrink-0">
          {expanded ? <ChevronDown className="h-4 w-4" /> : <ChevronRight className="h-4 w-4" />}
        </span>
        <div className="min-w-0 flex-1 space-y-1.5">
          <div className="flex flex-wrap items-center gap-1.5">
            <span className="font-medium truncate">{model.modelName || model.modelId}</span>
            {isDefault && <Badge variant="success">默认</Badge>}
            {thinking && (
              <Badge variant="secondary" className="gap-1">
                <Brain className="h-3 w-3" />
                Thinking
              </Badge>
            )}
            {model.rateMultiplier != null && (
              <Badge variant="outline">
                {model.rateMultiplier}x{model.rateUnit ? ` ${model.rateUnit}` : ''}
              </Badge>
            )}
            {model.apiBackend && <Badge variant="outline">{model.apiBackend}</Badge>}
          </div>
          <div className="font-mono text-xs text-muted-foreground truncate" title={model.modelId}>
            {model.modelId}
          </div>
          {model.description && (
            <p className="text-xs text-muted-foreground line-clamp-2">{model.description}</p>
          )}
          <div className="flex flex-wrap gap-x-3 gap-y-1 text-xs text-muted-foreground">
            <span>
              输入 {formatTokens(model.tokenLimits?.maxInputTokens)} / 输出{' '}
              {formatTokens(model.tokenLimits?.maxOutputTokens)}
            </span>
            {model.supportedInputTypes && model.supportedInputTypes.length > 0 && (
              <span>输入类型: {model.supportedInputTypes.join(', ')}</span>
            )}
            {efforts.length > 0 && <span>effort: {efforts.join(', ')}</span>}
          </div>
        </div>
      </button>

      {expanded && (
        <div className="border-t px-3 py-3 space-y-3 text-sm">
          <div className="grid grid-cols-1 sm:grid-cols-2 gap-3">
            <div>
              <div className="text-xs text-muted-foreground mb-1">Model ID</div>
              <div className="flex items-center gap-1 font-mono text-xs break-all">
                <span className="min-w-0">{model.modelId}</span>
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-6 w-6 shrink-0"
                  onClick={async (e) => {
                    e.stopPropagation()
                    try {
                      await navigator.clipboard.writeText(model.modelId)
                      toast.success('已复制 modelId')
                    } catch {
                      toast.error('复制失败')
                    }
                  }}
                  title="复制 modelId"
                >
                  <Copy className="h-3 w-3" />
                </Button>
              </div>
            </div>
            {model.apiBackend && (
              <div>
                <div className="text-xs text-muted-foreground mb-1">Grok API Backend</div>
                <div className="font-mono text-xs">{model.apiBackend}</div>
              </div>
            )}
            <div>
              <div className="text-xs text-muted-foreground mb-1">Token 限制</div>
              <div>
                最大输入: {model.tokenLimits?.maxInputTokens?.toLocaleString() ?? '-'}
                <br />
                最大输出: {model.tokenLimits?.maxOutputTokens?.toLocaleString() ?? '-'}
              </div>
            </div>
            <div>
              <div className="text-xs text-muted-foreground mb-1">费率</div>
              <div>
                {model.rateMultiplier != null ? `${model.rateMultiplier}x` : '-'}
                {model.rateUnit ? ` (${model.rateUnit})` : ''}
              </div>
            </div>
            <div>
              <div className="text-xs text-muted-foreground mb-1">Prompt Caching</div>
              <div>
                {model.promptCaching?.supportsPromptCaching
                  ? `支持 · 最小 ${model.promptCaching.minimumTokensPerCacheCheckpoint ?? '-'} tokens/检查点 · 最多 ${model.promptCaching.maximumCacheCheckpointsPerRequest ?? '-'} 检查点/请求`
                  : '不支持 / 未知'}
              </div>
            </div>
            {model.supportedInputTypes && model.supportedInputTypes.length > 0 && (
              <div>
                <div className="text-xs text-muted-foreground mb-1">支持输入类型</div>
                <div className="flex flex-wrap gap-1">
                  {model.supportedInputTypes.map((t) => (
                    <Badge key={t} variant="outline" className="text-[10px]">
                      {t}
                    </Badge>
                  ))}
                </div>
              </div>
            )}
            {fieldKeys.length > 0 && (
              <div>
                <div className="text-xs text-muted-foreground mb-1">额外请求字段</div>
                <div className="flex flex-wrap gap-1">
                  {fieldKeys.map((k) => (
                    <Badge key={k} variant="secondary" className="text-[10px]">
                      {k}
                    </Badge>
                  ))}
                </div>
              </div>
            )}
            {efforts.length > 0 && (
              <div>
                <div className="text-xs text-muted-foreground mb-1">Effort 等级</div>
                <div className="flex flex-wrap gap-1">
                  {efforts.map((e) => (
                    <Badge key={e} variant="outline" className="text-[10px]">
                      {e}
                    </Badge>
                  ))}
                </div>
                {defaultEffort && (
                  <div className="mt-1 text-xs text-muted-foreground">
                    默认: {defaultEffort}
                  </div>
                )}
              </div>
            )}
          </div>

          {schema && (
            <div>
              <div className="text-xs text-muted-foreground mb-1">
                additionalModelRequestFieldsSchema
              </div>
              <pre className="max-h-56 overflow-auto rounded-md bg-muted/60 p-3 text-[11px] leading-relaxed font-mono whitespace-pre-wrap break-all">
                {JSON.stringify(schema, null, 2)}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}

export function CatalogDialog({
  credentialId,
  credentialLabel,
  open,
  onOpenChange,
}: CatalogDialogProps) {
  const [search, setSearch] = useState('')
  const [thinkingOnly, setThinkingOnly] = useState(false)
  const [refreshing, setRefreshing] = useState(false)
  const queryClient = useQueryClient()
  const { data, isLoading, error, isFetching } = useCredentialCatalog(credentialId, open)

  const filtered = useMemo(() => {
    if (!data?.models) return []
    const q = search.trim().toLowerCase()
    return data.models.filter((m) => {
      if (thinkingOnly && !modelSupportsThinking(m)) return false
      if (!q) return true
      return (
        m.modelId.toLowerCase().includes(q) ||
        m.modelName.toLowerCase().includes(q) ||
        (m.description?.toLowerCase().includes(q) ?? false)
      )
    })
  }, [data?.models, search, thinkingOnly])

  const thinkingCount = useMemo(
    () => data?.models.filter(modelSupportsThinking).length ?? 0,
    [data?.models]
  )

  const handleRefresh = async () => {
    if (credentialId == null) return
    setRefreshing(true)
    try {
      const fresh = await getCredentialCatalog(credentialId, true)
      queryClient.setQueryData(['credential-catalog', credentialId], fresh)
      toast.success(`已从上游刷新（${fresh.models.length} 个模型）`)
    } catch (err) {
      toast.error('刷新失败: ' + (err as Error).message)
    } finally {
      setRefreshing(false)
    }
  }

  const handleCopyAll = async () => {
    if (!data) return
    try {
      await navigator.clipboard.writeText(JSON.stringify(data, null, 2))
      toast.success('已复制完整 catalog JSON')
    } catch {
      toast.error('复制失败')
    }
  }

  const titleLabel =
    credentialLabel || (credentialId != null ? `凭据 #${credentialId}` : '凭据')

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) {
          setSearch('')
          setThinkingOnly(false)
        }
        onOpenChange(next)
      }}
    >
      <DialogContent className="flex max-h-[90vh] w-full max-w-3xl flex-col gap-0 overflow-hidden p-0 sm:max-w-3xl">
        <DialogHeader className="shrink-0 space-y-1 border-b px-6 py-4 pr-12">
          <DialogTitle className="flex items-center gap-2">
            <BookOpen className="h-5 w-5" />
            模型目录 · {titleLabel}
          </DialogTitle>
          <DialogDescription>
            该凭据可用的模型列表（上游 ListAvailableModels）。内容可能较长，可搜索与展开详情。
          </DialogDescription>
        </DialogHeader>

        <div className="flex min-h-0 flex-1 flex-col">
          {/* 工具栏 */}
          <div className="shrink-0 space-y-3 border-b px-6 py-3">
            <div className="flex flex-wrap items-center gap-2">
              <div className="relative min-w-[200px] flex-1">
                <Search className="absolute left-2.5 top-1/2 h-4 w-4 -translate-y-1/2 text-muted-foreground" />
                <Input
                  value={search}
                  onChange={(e) => setSearch(e.target.value)}
                  placeholder="搜索 modelId / 名称 / 描述…"
                  className="pl-8 h-9"
                />
              </div>
              <Button
                size="sm"
                variant={thinkingOnly ? 'default' : 'outline'}
                onClick={() => setThinkingOnly((v) => !v)}
                className="shrink-0"
              >
                <Brain className="h-4 w-4 mr-1" />
                Thinking
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={handleRefresh}
                disabled={refreshing || isLoading || credentialId == null}
                title="强制向上游重新拉取"
              >
                <RefreshCw className={`h-4 w-4 mr-1 ${refreshing || isFetching ? 'animate-spin' : ''}`} />
                刷新
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={handleCopyAll}
                disabled={!data}
                title="复制完整 JSON"
              >
                <Copy className="h-4 w-4 mr-1" />
                JSON
              </Button>
            </div>

            {data && (
              <div className="flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                <Badge variant="outline">{data.models.length} 个模型</Badge>
                <Badge variant="secondary">Thinking {thinkingCount}</Badge>
                {data.defaultModel?.modelId && (
                  <span>
                    默认: <span className="font-mono text-foreground">{data.defaultModel.modelId}</span>
                  </span>
                )}
                <span>
                  来源:{' '}
                  <span className="text-foreground">
                    {data.source === 'cache' ? '内存缓存' : data.source === 'upstream' ? '上游实时' : data.source}
                  </span>
                </span>
                {filtered.length !== data.models.length && (
                  <span>当前显示 {filtered.length} 条</span>
                )}
              </div>
            )}
          </div>

          {/* 内容区 */}
          <div className="min-h-0 flex-1 overflow-y-auto px-6 py-4">
            {isLoading && (
              <div className="flex flex-col items-center justify-center gap-2 py-16 text-muted-foreground">
                <Loader2 className="h-8 w-8 animate-spin" />
                <span>正在加载模型目录…</span>
              </div>
            )}

            {error &&
              (() => {
                const parsed = parseError(error)
                return (
                  <div className="py-10 space-y-3 text-center">
                    <div className="font-medium text-destructive">{parsed.title}</div>
                    {parsed.detail && (
                      <div className="text-sm text-muted-foreground px-4 break-words">
                        {parsed.detail}
                      </div>
                    )}
                    <Button size="sm" variant="outline" onClick={handleRefresh} disabled={refreshing}>
                      <RefreshCw className={`h-4 w-4 mr-1 ${refreshing ? 'animate-spin' : ''}`} />
                      重试（强制刷新）
                    </Button>
                  </div>
                )
              })()}

            {data && !isLoading && filtered.length === 0 && (
              <div className="py-12 text-center text-muted-foreground text-sm">
                没有匹配的模型
              </div>
            )}

            {data && filtered.length > 0 && (
              <div className="space-y-2">
                {filtered.map((model) => (
                  <ModelCard
                    key={model.modelId}
                    model={model}
                    defaultModelId={data.defaultModel?.modelId}
                  />
                ))}
              </div>
            )}
          </div>
        </div>
      </DialogContent>
    </Dialog>
  )
}
