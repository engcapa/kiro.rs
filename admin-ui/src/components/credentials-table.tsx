import { useState } from 'react'
import { toast } from 'sonner'
import {
  ArrowUpDown,
  Check,
  ChevronDown,
  ChevronUp,
  Loader2,
  Pencil,
  RefreshCw,
  RotateCcw,
  Trash2,
  Wallet,
  X,
} from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import type { BalanceResponse, CredentialStatusItem } from '@/types/api'
import {
  useDeleteCredential,
  useForceRefreshToken,
  useResetFailure,
  useSetDisabled,
  useSetName,
  useSetPriority,
} from '@/hooks/use-credentials'

export type CredentialSortKey =
  | 'name'
  | 'id'
  | 'priority'
  | 'status'
  | 'authMethod'
  | 'endpoint'
  | 'profileArn'
  | 'importedAt'
  | 'lastUsedAt'
  | 'successCount'
  | 'failureCount'
  | 'remaining'

export type SortDirection = 'asc' | 'desc'

interface CredentialsTableProps {
  credentials: CredentialStatusItem[]
  selectedIds: Set<number>
  allSelected: boolean
  onToggleSelect: (id: number) => void
  onToggleSelectAll: () => void
  onViewBalance: (id: number) => void
  balanceMap: Map<number, BalanceResponse>
  loadingBalanceIds: Set<number>
  sortKey: CredentialSortKey
  sortDirection: SortDirection
  onSort: (key: CredentialSortKey) => void
}

interface CredentialRowProps {
  credential: CredentialStatusItem
  selected: boolean
  onToggleSelect: () => void
  onViewBalance: (id: number) => void
  balance: BalanceResponse | null
  loadingBalance: boolean
}

function formatAuthMethod(authMethod: string | null): string {
  if (authMethod === 'api_key') return 'API Key'
  if (authMethod === 'idc') return 'IdC'
  if (authMethod === 'social') return 'Social'
  return authMethod || '-'
}

function formatDateTime(value?: string | null): string {
  if (!value) return '-'
  const date = new Date(value)
  if (Number.isNaN(date.getTime())) return '-'
  return new Intl.DateTimeFormat('zh-CN', {
    month: '2-digit',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
  }).format(date)
}

function formatLastUsed(lastUsedAt: string | null): string {
  if (!lastUsedAt) return '从未'
  const date = new Date(lastUsedAt)
  const now = new Date()
  const diff = now.getTime() - date.getTime()
  if (diff < 0) return '刚刚'
  const seconds = Math.floor(diff / 1000)
  if (seconds < 60) return `${seconds} 秒前`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  return `${days} 天前`
}

function SortableHeader({
  label,
  sortKey,
  activeKey,
  direction,
  onSort,
  className = '',
}: {
  label: string
  sortKey: CredentialSortKey
  activeKey: CredentialSortKey
  direction: SortDirection
  onSort: (key: CredentialSortKey) => void
  className?: string
}) {
  const active = sortKey === activeKey
  return (
    <th className={`px-3 py-2 text-left font-medium ${className}`}>
      <button
        type="button"
        onClick={() => onSort(sortKey)}
        className="inline-flex items-center gap-1 whitespace-nowrap text-muted-foreground hover:text-foreground"
      >
        {label}
        {active ? (
          direction === 'asc' ? <ChevronUp className="h-3.5 w-3.5" /> : <ChevronDown className="h-3.5 w-3.5" />
        ) : (
          <ArrowUpDown className="h-3.5 w-3.5 opacity-60" />
        )}
      </button>
    </th>
  )
}

function CredentialRow({
  credential,
  selected,
  onToggleSelect,
  onViewBalance,
  balance,
  loadingBalance,
}: CredentialRowProps) {
  const [editingName, setEditingName] = useState(false)
  const [nameValue, setNameValue] = useState(credential.name)
  const [editingPriority, setEditingPriority] = useState(false)
  const [priorityValue, setPriorityValue] = useState(String(credential.priority))
  const [showDeleteDialog, setShowDeleteDialog] = useState(false)

  const setDisabled = useSetDisabled()
  const setName = useSetName()
  const setPriority = useSetPriority()
  const resetFailure = useResetFailure()
  const deleteCredential = useDeleteCredential()
  const forceRefresh = useForceRefreshToken()

  const displayName = credential.name || credential.userName || credential.email || `凭据 #${credential.id}`
  const hasFailures = credential.failureCount > 0 || credential.refreshFailureCount > 0

  const handleSaveName = () => {
    const nextName = nameValue.trim()
    if (!nextName) {
      toast.error('凭据名称不能为空')
      return
    }
    setName.mutate(
      { id: credential.id, name: nextName },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingName(false)
        },
        onError: (err) => toast.error('名称更新失败: ' + (err as Error).message),
      }
    )
  }

  const handlePriorityChange = () => {
    const newPriority = parseInt(priorityValue, 10)
    if (Number.isNaN(newPriority) || newPriority < 0) {
      toast.error('优先级必须是非负整数')
      return
    }
    setPriority.mutate(
      { id: credential.id, priority: newPriority },
      {
        onSuccess: (res) => {
          toast.success(res.message)
          setEditingPriority(false)
        },
        onError: (err) => toast.error('优先级更新失败: ' + (err as Error).message),
      }
    )
  }

  const handleToggleDisabled = () => {
    setDisabled.mutate(
      { id: credential.id, disabled: !credential.disabled },
      {
        onSuccess: (res) => toast.success(res.message),
        onError: (err) => toast.error('操作失败: ' + (err as Error).message),
      }
    )
  }

  const handleReset = () => {
    resetFailure.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error('恢复失败: ' + (err as Error).message),
    })
  }

  const handleForceRefresh = () => {
    forceRefresh.mutate(credential.id, {
      onSuccess: (res) => toast.success(res.message),
      onError: (err) => toast.error('刷新失败: ' + (err as Error).message),
    })
  }

  const handleDelete = () => {
    if (!credential.disabled) {
      toast.error('请先禁用凭据再删除')
      setShowDeleteDialog(false)
      return
    }

    deleteCredential.mutate(credential.id, {
      onSuccess: (res) => {
        toast.success(res.message)
        setShowDeleteDialog(false)
      },
      onError: (err) => toast.error('删除失败: ' + (err as Error).message),
    })
  }

  return (
    <>
      <tr className={credential.isCurrent ? 'bg-primary/5' : undefined}>
        <td className="w-10 px-3 py-3 align-middle">
          <Checkbox checked={selected} onCheckedChange={onToggleSelect} />
        </td>
        <td className="min-w-[240px] px-3 py-3 align-middle">
          <div className="flex min-w-0 items-center gap-2">
            {editingName ? (
              <div className="flex min-w-[220px] items-center gap-1">
                <Input
                  value={nameValue}
                  onChange={(event) => setNameValue(event.target.value)}
                  className="h-8 min-w-0"
                  disabled={setName.isPending}
                />
                <Button size="icon" variant="ghost" className="h-8 w-8" onClick={handleSaveName} disabled={setName.isPending}>
                  {setName.isPending ? <Loader2 className="h-4 w-4 animate-spin" /> : <Check className="h-4 w-4" />}
                </Button>
                <Button
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8"
                  onClick={() => {
                    setEditingName(false)
                    setNameValue(credential.name)
                  }}
                >
                  <X className="h-4 w-4" />
                </Button>
              </div>
            ) : (
              <>
                <button
                  type="button"
                  onClick={() => setEditingName(true)}
                  className="min-w-0 truncate text-left font-medium hover:underline"
                  title={displayName}
                >
                  {displayName}
                </button>
                <Button size="icon" variant="ghost" className="h-7 w-7" onClick={() => setEditingName(true)} title="编辑名称">
                  <Pencil className="h-3.5 w-3.5" />
                </Button>
              </>
            )}
          </div>
          <div className="mt-1 flex flex-wrap items-center gap-1 text-xs text-muted-foreground">
            <span>#{credential.id}</span>
            {credential.email && <span className="truncate">{credential.email}</span>}
            {credential.userName && credential.userName !== credential.email && <span className="truncate">{credential.userName}</span>}
          </div>
        </td>
        <td className="px-3 py-3 align-middle">
          <div className="flex flex-wrap gap-1">
            {credential.isCurrent && <Badge variant="success">当前</Badge>}
            {credential.disabled ? <Badge variant="destructive">禁用</Badge> : <Badge variant="outline">启用</Badge>}
            {credential.disabledReason && <Badge variant="outline">{credential.disabledReason}</Badge>}
            {credential.pools && credential.pools.length > 0 && (
              <div className="flex gap-1 flex-wrap mt-1 w-full">
                {credential.pools.map((pool) => (
                  <Badge key={pool} variant="secondary" className="text-[10px] px-1 py-0 h-4">
                    {pool}
                  </Badge>
                ))}
              </div>
            )}
          </div>
        </td>
        <td className="px-3 py-3 align-middle">
          <div className="flex flex-col gap-1 text-sm">
            <span>{formatAuthMethod(credential.authMethod)}</span>
            <span className="text-xs text-muted-foreground">{credential.endpoint}</span>
            {credential.hasProxy && <span className="max-w-[180px] truncate text-xs text-muted-foreground" title={credential.proxyUrl}>代理 {credential.proxyUrl}</span>}
          </div>
        </td>
        <td className="px-3 py-3 align-middle">
          {editingPriority ? (
            <div className="flex items-center gap-1">
              <Input
                type="number"
                min="0"
                value={priorityValue}
                onChange={(event) => setPriorityValue(event.target.value)}
                className="h-8 w-20"
                disabled={setPriority.isPending}
              />
              <Button size="icon" variant="ghost" className="h-8 w-8" onClick={handlePriorityChange} disabled={setPriority.isPending}>
                <Check className="h-4 w-4" />
              </Button>
              <Button
                size="icon"
                variant="ghost"
                className="h-8 w-8"
                onClick={() => {
                  setEditingPriority(false)
                  setPriorityValue(String(credential.priority))
                }}
              >
                <X className="h-4 w-4" />
              </Button>
            </div>
          ) : (
            <button type="button" onClick={() => setEditingPriority(true)} className="font-medium hover:underline">
              {credential.priority}
            </button>
          )}
        </td>
        <td className="px-3 py-3 align-middle text-sm">
          {loadingBalance ? (
            <span className="inline-flex items-center gap-1 text-muted-foreground">
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
              查询中
            </span>
          ) : balance ? (
            <div className="space-y-1">
              <div className="font-medium">{balance.remaining.toFixed(2)} / {balance.usageLimit.toFixed(2)}</div>
              <div className="text-xs text-muted-foreground">{balance.subscriptionTitle || '未知订阅'}，剩余 {(100 - balance.usagePercentage).toFixed(1)}%</div>
            </div>
          ) : (
            <span className="text-muted-foreground">未知</span>
          )}
        </td>
        <td className="px-3 py-3 align-middle text-sm">
          <div className="space-y-1">
            <div>成功 {credential.successCount}</div>
            <div className={hasFailures ? 'text-destructive' : 'text-muted-foreground'}>
              失败 {credential.failureCount} / 刷新 {credential.refreshFailureCount}
            </div>
          </div>
        </td>
        <td className="max-w-[220px] px-3 py-3 align-middle">
          {credential.profileArn ? (
            <span className="block truncate font-mono text-xs" title={credential.profileArn}>
              {credential.profileArn}
            </span>
          ) : (
            <Badge variant="warning">缺失</Badge>
          )}
        </td>
        <td className="whitespace-nowrap px-3 py-3 align-middle text-sm text-muted-foreground">
          {formatDateTime(credential.importedAt)}
        </td>
        <td className="whitespace-nowrap px-3 py-3 align-middle text-sm text-muted-foreground">
          {formatLastUsed(credential.lastUsedAt)}
        </td>
        <td className="min-w-[190px] px-3 py-3 align-middle">
          <div className="flex items-center gap-1">
            <Switch checked={!credential.disabled} onCheckedChange={handleToggleDisabled} disabled={setDisabled.isPending} title="启用/禁用" />
            <Button size="icon" variant="ghost" className="h-8 w-8" onClick={() => onViewBalance(credential.id)} title="查看余额">
              <Wallet className="h-4 w-4" />
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              onClick={handleForceRefresh}
              disabled={forceRefresh.isPending || credential.disabled || credential.authMethod === 'api_key'}
              title={credential.authMethod === 'api_key' ? 'API Key 凭据无需刷新 Token' : '刷新 Token'}
            >
              <RefreshCw className={`h-4 w-4 ${forceRefresh.isPending ? 'animate-spin' : ''}`} />
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8"
              onClick={handleReset}
              disabled={resetFailure.isPending || !hasFailures}
              title="恢复异常"
            >
              <RotateCcw className="h-4 w-4" />
            </Button>
            <Button
              size="icon"
              variant="ghost"
              className="h-8 w-8 text-destructive hover:text-destructive"
              onClick={() => setShowDeleteDialog(true)}
              disabled={!credential.disabled}
              title={!credential.disabled ? '需要先禁用凭据才能删除' : '删除'}
            >
              <Trash2 className="h-4 w-4" />
            </Button>
          </div>
        </td>
      </tr>

      <Dialog open={showDeleteDialog} onOpenChange={setShowDeleteDialog}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>确认删除凭据</DialogTitle>
            <DialogDescription>
              确定删除 {displayName} 吗？此操作无法撤销。
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button variant="outline" onClick={() => setShowDeleteDialog(false)} disabled={deleteCredential.isPending}>
              取消
            </Button>
            <Button variant="destructive" onClick={handleDelete} disabled={deleteCredential.isPending || !credential.disabled}>
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  )
}

export function CredentialsTable({
  credentials,
  selectedIds,
  allSelected,
  onToggleSelect,
  onToggleSelectAll,
  onViewBalance,
  balanceMap,
  loadingBalanceIds,
  sortKey,
  sortDirection,
  onSort,
}: CredentialsTableProps) {
  return (
    <div className="overflow-hidden rounded-md border bg-background">
      <div className="overflow-x-auto">
        <table className="w-full min-w-[1280px] border-collapse text-sm">
          <thead className="border-b bg-muted/45">
            <tr>
              <th className="w-10 px-3 py-2 text-left">
                <Checkbox checked={allSelected} onCheckedChange={onToggleSelectAll} />
              </th>
              <SortableHeader label="名称" sortKey="name" activeKey={sortKey} direction={sortDirection} onSort={onSort} />
              <SortableHeader label="状态与池" sortKey="status" activeKey={sortKey} direction={sortDirection} onSort={onSort} />
              <SortableHeader label="认证" sortKey="authMethod" activeKey={sortKey} direction={sortDirection} onSort={onSort} />
              <SortableHeader label="优先级" sortKey="priority" activeKey={sortKey} direction={sortDirection} onSort={onSort} />
              <SortableHeader label="余额" sortKey="remaining" activeKey={sortKey} direction={sortDirection} onSort={onSort} />
              <SortableHeader label="统计" sortKey="successCount" activeKey={sortKey} direction={sortDirection} onSort={onSort} />
              <SortableHeader label="Profile ARN" sortKey="profileArn" activeKey={sortKey} direction={sortDirection} onSort={onSort} />
              <SortableHeader label="导入时间" sortKey="importedAt" activeKey={sortKey} direction={sortDirection} onSort={onSort} />
              <SortableHeader label="最后调用" sortKey="lastUsedAt" activeKey={sortKey} direction={sortDirection} onSort={onSort} />
              <th className="px-3 py-2 text-left font-medium text-muted-foreground">操作</th>
            </tr>
          </thead>
          <tbody className="divide-y">
            {credentials.map((credential) => (
              <CredentialRow
                key={credential.id}
                credential={credential}
                selected={selectedIds.has(credential.id)}
                onToggleSelect={() => onToggleSelect(credential.id)}
                onViewBalance={onViewBalance}
                balance={balanceMap.get(credential.id) || null}
                loadingBalance={loadingBalanceIds.has(credential.id)}
              />
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}
