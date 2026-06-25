import { useState } from 'react'
import { toast } from 'sonner'
import { Plus, Trash2, Pencil, Check, X, Loader2 } from 'lucide-react'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
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
} from '@/hooks/use-api-keys'
import { extractErrorMessage } from '@/lib/utils'
import type { ApiKeyEntry } from '@/types/api'

export function ApiKeysManager() {
  const { data, isLoading } = useApiKeys()
  const [addDialogOpen, setAddDialogOpen] = useState(false)

  const keys = data?.keys || []

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
        <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
          {keys.map(apiKey => (
            <ApiKeyCard key={apiKey.id} apiKey={apiKey} />
          ))}
        </div>
      )}

      <AddApiKeyDialog open={addDialogOpen} onOpenChange={setAddDialogOpen} />
    </div>
  )
}

function ApiKeyCard({ apiKey }: { apiKey: ApiKeyEntry }) {
  const [editingName, setEditingName] = useState(false)
  const [nameValue, setNameValue] = useState(apiKey.name)
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)

  const updateApiKey = useUpdateApiKey()
  const deleteApiKey = useDeleteApiKey()

  const handleSaveName = () => {
    const nextName = nameValue.trim()
    if (!nextName) {
      toast.error('名称不能为空')
      return
    }
    updateApiKey.mutate(
      { id: apiKey.id, req: { name: nextName } },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingName(false)
        },
        onError: (err) => toast.error('更新失败: ' + extractErrorMessage(err)),
      }
    )
  }

  const handleToggleDisabled = () => {
    updateApiKey.mutate(
      { id: apiKey.id, req: { disabled: !apiKey.disabled } },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error('更新失败: ' + extractErrorMessage(err)),
      }
    )
  }

  const handleDelete = () => {
    deleteApiKey.mutate(apiKey.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDeleteDialog(false)
      },
      onError: (err) => toast.error('删除失败: ' + extractErrorMessage(err)),
    })
  }

  return (
    <>
      <Card>
        <CardHeader className="pb-2">
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-2 max-w-[70%]">
              {editingName ? (
                <div className="flex items-center gap-1">
                  <Input
                    value={nameValue}
                    onChange={(e) => setNameValue(e.target.value)}
                    className="h-7 text-sm"
                  />
                  <Button size="icon" variant="ghost" className="h-7 w-7" onClick={handleSaveName} disabled={updateApiKey.isPending}>
                    {updateApiKey.isPending ? <Loader2 className="h-3 w-3 animate-spin" /> : <Check className="h-3 w-3" />}
                  </Button>
                  <Button size="icon" variant="ghost" className="h-7 w-7" onClick={() => { setEditingName(false); setNameValue(apiKey.name) }}>
                    <X className="h-3 w-3" />
                  </Button>
                </div>
              ) : (
                <>
                  <CardTitle className="text-lg truncate" title={apiKey.name}>{apiKey.name}</CardTitle>
                  <Button size="icon" variant="ghost" className="h-6 w-6" onClick={() => setEditingName(true)}>
                    <Pencil className="h-3 w-3" />
                  </Button>
                </>
              )}
            </div>
            <div className="flex items-center gap-2">
              <span className="text-sm text-muted-foreground">启用</span>
              <Switch
                checked={!apiKey.disabled}
                onCheckedChange={handleToggleDisabled}
                disabled={updateApiKey.isPending}
              />
            </div>
          </div>
        </CardHeader>
        <CardContent className="space-y-3">
          <div>
            <span className="text-sm text-muted-foreground">API Key: </span>
            <code className="text-sm font-mono bg-muted px-1 py-0.5 rounded">{apiKey.key}</code>
          </div>
          <div>
            <span className="text-sm text-muted-foreground block mb-1">权限池 (Pools):</span>
            <div className="flex gap-1 flex-wrap">
              {apiKey.pools && apiKey.pools.length > 0 ? (
                apiKey.pools.map(pool => (
                  <Badge key={pool} variant="secondary" className="text-xs">{pool}</Badge>
                ))
              ) : (
                <span className="text-sm text-muted-foreground">无</span>
              )}
            </div>
          </div>
          {apiKey.createdAt && (
            <div>
              <span className="text-xs text-muted-foreground">创建时间: {new Date(apiKey.createdAt).toLocaleString()}</span>
            </div>
          )}
          <div className="pt-2 flex justify-end">
            <Button
              size="sm"
              variant="destructive"
              onClick={() => setShowDeleteDialog(true)}
            >
              <Trash2 className="h-4 w-4 mr-1" />
              删除
            </Button>
          </div>
        </CardContent>
      </Card>

      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除 API Key</DialogTitle>
          </DialogHeader>
          <div className="py-4">
            确定要删除 API Key <strong>{apiKey.name}</strong> 吗？此操作无法撤销。
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setShowDeleteDialog(false)} disabled={deleteApiKey.isPending}>
              取消
            </Button>
            <Button variant="destructive" onClick={handleDelete} disabled={deleteApiKey.isPending}>
              {deleteApiKey.isPending ? '删除中...' : '确认删除'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

function AddApiKeyDialog({ open, onOpenChange }: { open: boolean, onOpenChange: (open: boolean) => void }) {
  const [name, setName] = useState('')
  const [key, setKey] = useState('')
  const [pools, setPools] = useState('')

  const { mutate, isPending } = useAddApiKey()

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
        pools: pools.split(',').map(p => p.trim()).filter(Boolean),
      },
      {
        onSuccess: (data) => {
          toast.success(data.message)
          if (data.key) {
            toast.info(`生成的 API Key: ${data.key}`, { duration: 10000 })
          }
          onOpenChange(false)
          setName('')
          setKey('')
          setPools('')
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
          <div className="space-y-2">
            <label className="text-sm font-medium">权限池 (Pools)</label>
            <Input
              value={pools}
              onChange={e => setPools(e.target.value)}
              placeholder="逗号分隔，例如: default,pro"
              disabled={isPending}
            />
          </div>
          <DialogFooter>
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
