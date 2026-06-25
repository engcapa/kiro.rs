import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { Plus, Trash2, Pencil, Check, Loader2, Copy } from 'lucide-react'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import {
  useApiKeys,
  useAddApiKey,
  useUpdateApiKey,
  useDeleteApiKey,
  usePools,
} from '@/hooks/use-api-keys'
import { extractErrorMessage } from '@/lib/utils'
import type { ApiKeyEntry } from '@/types/api'

export function ApiKeysManager() {
  const { data, isLoading } = useApiKeys()
  const { data: poolsData } = usePools()
  const [addDialogOpen, setAddDialogOpen] = useState(false)
  const [editDialogOpen, setEditDialogOpen] = useState(false)
  const [editingApiKey, setEditingApiKey] = useState<ApiKeyEntry | null>(null)
  const [deleteDialogOpen, setDeleteDialogOpen] = useState(false)
  const [deletingApiKey, setDeletingApiKey] = useState<ApiKeyEntry | null>(null)

  const deleteApiKeyMutation = useDeleteApiKey()

  const keys = data?.keys || []
  const availablePools = poolsData || []

  const handleDeleteConfirm = () => {
    if (!deletingApiKey) return
    deleteApiKeyMutation.mutate(deletingApiKey.id, {
      onSuccess: (res) => {
        toast.success(res.message || '删除成功')
        setDeleteDialogOpen(false)
        setDeletingApiKey(null)
      },
      onError: (err) => toast.error('删除失败: ' + extractErrorMessage(err)),
    })
  }

  if (isLoading) {
    return (
      <Card>
        <CardContent className="py-8 text-center text-muted-foreground">
          <Loader2 className="h-6 w-6 animate-spin mx-auto mb-2" />
          加载中...
        </CardContent>
      </Card>
    )
  }

  return (
    <div className="space-y-4">
      <div className="flex justify-between items-center">
        <h2 className="text-xl font-semibold">API Keys 管理</h2>
        <Button onClick={() => setAddDialogOpen(true)} size="sm">
          <Plus className="h-4 w-4 mr-2" />
          添加 API Key
        </Button>
      </div>

      {keys.length === 0 ? (
        <Card>
          <CardContent className="py-8 text-center text-muted-foreground">
            暂无 API Key
          </CardContent>
        </Card>
      ) : (
        <div className="rounded-md border bg-card">
          <div className="overflow-x-auto">
            <table className="w-full border-collapse text-sm">
              <thead>
                <tr className="border-b bg-muted/40 transition-colors">
                  <th className="h-10 px-4 text-left align-middle font-medium text-muted-foreground w-20">ID</th>
                  <th className="h-10 px-4 text-left align-middle font-medium text-muted-foreground min-w-[120px]">名称</th>
                  <th className="h-10 px-4 text-left align-middle font-medium text-muted-foreground min-w-[200px]">API Key</th>
                  <th className="h-10 px-4 text-left align-middle font-medium text-muted-foreground">权限池 (Pools)</th>
                  <th className="h-10 px-4 text-left align-middle font-medium text-muted-foreground w-[120px]">状态</th>
                  <th className="h-10 px-4 text-left align-middle font-medium text-muted-foreground w-[180px]">创建时间</th>
                  <th className="h-10 px-4 text-right align-middle font-medium text-muted-foreground w-[160px]">操作</th>
                </tr>
              </thead>
              <tbody className="[&_tr:last-child]:border-0">
                {keys.map(apiKey => (
                  <ApiKeyRow
                    key={apiKey.id}
                    apiKey={apiKey}
                    onEdit={(key) => {
                      setEditingApiKey(key)
                      setEditDialogOpen(true)
                    }}
                    onDeleteClick={(key) => {
                      setDeletingApiKey(key)
                      setDeleteDialogOpen(true)
                    }}
                  />
                ))}
              </tbody>
            </table>
          </div>
        </div>
      )}

      {/* Dialogs */}
      <AddApiKeyDialog
        open={addDialogOpen}
        onOpenChange={setAddDialogOpen}
        availablePools={availablePools}
      />

      <EditApiKeyDialog
        open={editDialogOpen}
        onOpenChange={setEditDialogOpen}
        apiKey={editingApiKey}
        availablePools={availablePools}
      />

      <Dialog open={deleteDialogOpen} onOpenChange={setDeleteDialogOpen}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除 API Key</DialogTitle>
          </DialogHeader>
          <div className="py-4">
            确定要删除 API Key <strong>{deletingApiKey?.name}</strong> 吗？此操作无法撤销。
          </div>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setDeleteDialogOpen(false)}
              disabled={deleteApiKeyMutation.isPending}
            >
              取消
            </Button>
            <Button
              variant="destructive"
              onClick={handleDeleteConfirm}
              disabled={deleteApiKeyMutation.isPending}
            >
              {deleteApiKeyMutation.isPending ? '删除中...' : '确认删除'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}

function ApiKeyRow({
  apiKey,
  onEdit,
  onDeleteClick,
}: {
  apiKey: ApiKeyEntry
  onEdit: (apiKey: ApiKeyEntry) => void
  onDeleteClick: (apiKey: ApiKeyEntry) => void
}) {
  const updateApiKey = useUpdateApiKey()

  const handleToggleDisabled = () => {
    updateApiKey.mutate(
      { id: apiKey.id, req: { disabled: !apiKey.disabled } },
      {
        onSuccess: () => toast.success(!apiKey.disabled ? 'API Key 已禁用' : 'API Key 已启用'),
        onError: (err) => toast.error('更新失败: ' + extractErrorMessage(err)),
      }
    )
  }

  return (
    <tr className="border-b transition-colors hover:bg-muted/50 data-[state=selected]:bg-muted">
      <td className="px-4 py-3 align-middle font-medium">#{apiKey.id}</td>
      <td className="px-4 py-3 align-middle font-medium max-w-[200px] truncate" title={apiKey.name}>
        {apiKey.name}
      </td>
      <td className="px-4 py-3 align-middle">
        <div className="flex items-center gap-2">
          <code className="font-mono bg-muted px-1.5 py-0.5 rounded text-xs select-all">
            {maskKey(apiKey.key)}
          </code>
          <CopyButton value={apiKey.key} />
        </div>
      </td>
      <td className="px-4 py-3 align-middle">
        <div className="flex gap-1 flex-wrap">
          {apiKey.pools && apiKey.pools.length > 0 ? (
            apiKey.pools.map(pool => (
              <Badge key={pool} variant="secondary" className="text-xs">
                {pool}
              </Badge>
            ))
          ) : (
            <span className="text-xs text-muted-foreground">无</span>
          )}
        </div>
      </td>
      <td className="px-4 py-3 align-middle">
        <div className="flex items-center gap-2">
          <Switch
            checked={!apiKey.disabled}
            onCheckedChange={handleToggleDisabled}
            disabled={updateApiKey.isPending}
          />
          <span className="text-xs text-muted-foreground">
            {apiKey.disabled ? '已禁用' : '已启用'}
          </span>
        </div>
      </td>
      <td className="px-4 py-3 align-middle text-xs text-muted-foreground">
        {apiKey.createdAt ? new Date(apiKey.createdAt).toLocaleString() : '-'}
      </td>
      <td className="px-4 py-3 align-middle text-right">
        <div className="flex justify-end gap-2">
          <Button
            size="icon"
            variant="outline"
            onClick={() => onEdit(apiKey)}
            className="h-8 w-8"
            title="编辑"
          >
            <Pencil className="h-4 w-4" />
          </Button>
          <Button
            size="icon"
            variant="destructive"
            onClick={() => onDeleteClick(apiKey)}
            className="h-8 w-8"
            title="删除"
          >
            <Trash2 className="h-4 w-4" />
          </Button>
        </div>
      </td>
    </tr>
  )
}

function CopyButton({ value }: { value: string }) {
  const [copied, setCopied] = useState(false)
  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(value)
      setCopied(true)
      toast.success('已复制到剪贴板')
      setTimeout(() => setCopied(false), 2000)
    } catch (err) {
      toast.error('复制失败')
    }
  }
  return (
    <Button
      size="icon"
      variant="ghost"
      className="h-8 w-8 hover:bg-muted"
      onClick={handleCopy}
      title="复制 API Key"
    >
      {copied ? (
        <Check className="h-4 w-4 text-green-500" />
      ) : (
        <Copy className="h-4 w-4 text-muted-foreground hover:text-foreground" />
      )}
    </Button>
  )
}

function maskKey(key: string): string {
  if (!key) return ''
  if (key.length <= 12) return '••••••••'
  const prefix = key.startsWith('ksk_') ? 'ksk_' : ''
  const rest = key.startsWith('ksk_') ? key.substring(4) : key
  if (rest.length <= 8) return prefix + '••••••••'
  return `${prefix}${rest.substring(0, 4)}••••••••${rest.substring(rest.length - 4)}`
}

interface PoolsSelectorProps {
  selectedPools: string[]
  onChange: (pools: string[]) => void
  availablePools: string[]
}

function PoolsSelector({
  selectedPools,
  onChange,
  availablePools,
}: PoolsSelectorProps) {
  const [newPoolInput, setNewPoolInput] = useState('')

  const handleTogglePool = (pool: string) => {
    if (selectedPools.includes(pool)) {
      onChange(selectedPools.filter(p => p !== pool))
    } else {
      onChange([...selectedPools, pool])
    }
  }

  const handleAddNewPool = (e: React.FormEvent) => {
    e.preventDefault()
    const pool = newPoolInput.trim()
    if (!pool) return
    if (!selectedPools.includes(pool)) {
      onChange([...selectedPools, pool])
    }
    setNewPoolInput('')
  }

  // Combine available pools and selected pools to get the full list of options
  const allOptions = Array.from(new Set([...availablePools, ...selectedPools])).filter(Boolean)

  return (
    <div className="space-y-3">
      <label className="text-sm font-medium">分配权限池 (Pools)</label>
      
      {allOptions.length === 0 ? (
        <p className="text-xs text-muted-foreground">暂无可用的凭据池，请在下方新增</p>
      ) : (
        <div className="grid grid-cols-2 gap-2 p-3 border rounded-md bg-muted/20 max-h-[160px] overflow-y-auto">
          {allOptions.map(pool => {
            const isChecked = selectedPools.includes(pool)
            return (
              <div key={pool} className="flex items-center space-x-2">
                <Checkbox
                  id={`pool-${pool}`}
                  checked={isChecked}
                  onCheckedChange={() => handleTogglePool(pool)}
                />
                <label
                  htmlFor={`pool-${pool}`}
                  className="text-sm font-medium leading-none peer-disabled:cursor-not-allowed peer-disabled:opacity-70 cursor-pointer select-none truncate"
                  title={pool}
                >
                  {pool}
                </label>
              </div>
            )
          })}
        </div>
      )}

      {/* Input to add a new custom pool */}
      <div className="flex gap-2">
        <Input
          value={newPoolInput}
          onChange={e => setNewPoolInput(e.target.value)}
          placeholder="输入新资源池名称"
          className="h-9"
          onKeyDown={e => {
            if (e.key === 'Enter') {
              e.preventDefault()
              handleAddNewPool(e)
            }
          }}
        />
        <Button
          type="button"
          variant="outline"
          size="sm"
          onClick={handleAddNewPool}
          className="h-9 whitespace-nowrap"
        >
          添加新池
        </Button>
      </div>
    </div>
  )
}

function AddApiKeyDialog({
  open,
  onOpenChange,
  availablePools,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  availablePools: string[]
}) {
  const [name, setName] = useState('')
  const [key, setKey] = useState('')
  const [selectedPools, setSelectedPools] = useState<string[]>([])

  const { mutate, isPending } = useAddApiKey()

  // Reset form when dialog opens/closes
  useEffect(() => {
    if (open) {
      setName('')
      setKey('')
      setSelectedPools([])
    }
  }, [open])

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()

    if (!name.trim()) {
      toast.error('请输入名称')
      return
    }

    mutate(
      {
        name: name.trim(),
        key: key.trim() || undefined,
        pools: selectedPools,
      },
      {
        onSuccess: (data) => {
          toast.success('API Key 已添加')
          if (data.key) {
            toast.info(`API Key: ${data.key}`, { duration: 10000 })
          }
          onOpenChange(false)
        },
        onError: (err) => toast.error('添加失败: ' + extractErrorMessage(err)),
      }
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>添加 API Key</DialogTitle>
        </DialogHeader>
        <form onSubmit={handleSubmit} className="space-y-4">
          <div className="space-y-2">
            <label className="text-sm font-medium">名称 <span className="text-red-500">*</span></label>
            <Input
              value={name}
              onChange={e => setName(e.target.value)}
              placeholder="例如: Frontend Client"
              disabled={isPending}
            />
          </div>
          <div className="space-y-2">
            <label className="text-sm font-medium">自定义 Key (可选)</label>
            <Input
              value={key}
              onChange={e => setKey(e.target.value)}
              placeholder="留空则自动生成"
              disabled={isPending}
            />
          </div>

          <PoolsSelector
            selectedPools={selectedPools}
            onChange={setSelectedPools}
            availablePools={availablePools}
          />

          <DialogFooter className="pt-2">
            <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={isPending}>
              取消
            </Button>
            <Button type="submit" disabled={isPending}>
              {isPending ? '添加中...' : '添加'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}

function EditApiKeyDialog({
  open,
  onOpenChange,
  apiKey,
  availablePools,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  apiKey: ApiKeyEntry | null
  availablePools: string[]
}) {
  const [name, setName] = useState('')
  const [selectedPools, setSelectedPools] = useState<string[]>([])
  const updateApiKey = useUpdateApiKey()

  // Initialize form values when dialog opens with a specific key
  useEffect(() => {
    if (apiKey && open) {
      setName(apiKey.name)
      setSelectedPools(apiKey.pools || [])
    }
  }, [apiKey, open])

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()
    if (!apiKey) return

    if (!name.trim()) {
      toast.error('请输入名称')
      return
    }

    updateApiKey.mutate(
      {
        id: apiKey.id,
        req: {
          name: name.trim(),
          pools: selectedPools,
        },
      },
      {
        onSuccess: () => {
          toast.success('API Key 已更新')
          onOpenChange(false)
        },
        onError: (err) => toast.error('更新失败: ' + extractErrorMessage(err)),
      }
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>编辑 API Key</DialogTitle>
        </DialogHeader>
        {apiKey && (
          <form onSubmit={handleSubmit} className="space-y-4">
            <div className="space-y-2">
              <label className="text-sm font-medium">名称 <span className="text-red-500">*</span></label>
              <Input
                value={name}
                onChange={e => setName(e.target.value)}
                placeholder="例如: Frontend Client"
                disabled={updateApiKey.isPending}
              />
            </div>
            <div className="space-y-2">
              <label className="text-sm font-medium block">API Key (只读)</label>
              <code className="text-sm font-mono bg-muted px-2 py-1 rounded block truncate select-all">
                {apiKey.key}
              </code>
            </div>
            
            <PoolsSelector
              selectedPools={selectedPools}
              onChange={setSelectedPools}
              availablePools={availablePools}
            />

            <DialogFooter className="pt-2">
              <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={updateApiKey.isPending}>
                取消
              </Button>
              <Button type="submit" disabled={updateApiKey.isPending}>
                {updateApiKey.isPending ? '保存中...' : '保存修改'}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  )
}
