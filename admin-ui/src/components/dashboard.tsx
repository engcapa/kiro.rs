import { useState, useEffect, useMemo, useRef } from 'react'
import { RefreshCw, LogOut, Moon, Sun, Server, Plus, Upload, FileUp, Trash2, RotateCcw, CheckCircle2, Tags, ShieldCheck } from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import { storage } from '@/lib/storage'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { CredentialsTable, type CredentialSortKey, type SortDirection } from '@/components/credentials-table'
import { BalanceDialog } from '@/components/balance-dialog'
import { CatalogDialog } from '@/components/catalog-dialog'
import { AddCredentialDialog } from '@/components/add-credential-dialog'
import { GrokOAuthDialog } from '@/components/grok-oauth-dialog'
import { BatchImportDialog } from '@/components/batch-import-dialog'
import { KamImportDialog } from '@/components/kam-import-dialog'
import { BatchVerifyDialog, type VerifyResult } from '@/components/batch-verify-dialog'
import { useCredentials, useDeleteCredential, useResetFailure, useLoadBalancingMode, useSetLoadBalancingMode, useSetCredentialPools } from '@/hooks/use-credentials'
import { getCredentialBalance, forceRefreshToken } from '@/api/credentials'
import { usePools } from '@/hooks/use-api-keys'
import { Checkbox } from '@/components/ui/checkbox'
import { Dialog, DialogContent, DialogHeader, DialogTitle, DialogFooter } from '@/components/ui/dialog'
import { extractErrorMessage } from '@/lib/utils'
import type { BalanceResponse, CredentialStatusItem, LoadBalancingMode } from '@/types/api'
import { ApiKeysManager } from '@/components/api-keys-manager'

interface DashboardProps {
  onLogout: () => void
}

const LOAD_BALANCING_MODES: LoadBalancingMode[] = ['round_robin', 'priority', 'balanced']

const LOAD_BALANCING_LABELS: Record<LoadBalancingMode, string> = {
  round_robin: '轮询模式',
  priority: '优先级模式',
  balanced: '均衡负载',
}

type StatusFilter = 'all' | 'enabled' | 'disabled' | 'current' | 'failed'
type ProfileFilter = 'all' | 'has' | 'missing'

function nextLoadBalancingMode(currentMode: LoadBalancingMode): LoadBalancingMode {
  const index = LOAD_BALANCING_MODES.indexOf(currentMode)
  return LOAD_BALANCING_MODES[(index + 1) % LOAD_BALANCING_MODES.length]
}

function credentialSearchText(credential: CredentialStatusItem): string {
  return [
    credential.id,
    credential.name,
    credential.email,
    credential.userName,
    credential.authMethod,
    credential.endpoint,
    credential.profileArn,
    credential.maskedApiKey,
    credential.proxyUrl,
  ]
    .filter(Boolean)
    .join(' ')
    .toLowerCase()
}

function dateValue(value?: string | null): number {
  if (!value) return 0
  const time = new Date(value).getTime()
  return Number.isNaN(time) ? 0 : time
}

export function Dashboard({ onLogout }: DashboardProps) {
  const [selectedCredentialId, setSelectedCredentialId] = useState<number | null>(null)
  const [balanceDialogOpen, setBalanceDialogOpen] = useState(false)
  const [catalogDialogOpen, setCatalogDialogOpen] = useState(false)
  const [addDialogOpen, setAddDialogOpen] = useState(false)
  const [grokOauthDialogOpen, setGrokOauthDialogOpen] = useState(false)
  const [batchImportDialogOpen, setBatchImportDialogOpen] = useState(false)
  const [kamImportDialogOpen, setKamImportDialogOpen] = useState(false)
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set())
  const [verifyDialogOpen, setVerifyDialogOpen] = useState(false)
  const [verifying, setVerifying] = useState(false)
  const [verifyProgress, setVerifyProgress] = useState({ current: 0, total: 0 })
  const [verifyResults, setVerifyResults] = useState<Map<number, VerifyResult>>(new Map())
  const [balanceMap, setBalanceMap] = useState<Map<number, BalanceResponse>>(new Map())
  const [loadingBalanceIds, setLoadingBalanceIds] = useState<Set<number>>(new Set())
  const [queryingInfo, setQueryingInfo] = useState(false)
  const [queryInfoProgress, setQueryInfoProgress] = useState({ current: 0, total: 0 })
  const [batchRefreshing, setBatchRefreshing] = useState(false)
  const [batchRefreshProgress, setBatchRefreshProgress] = useState({ current: 0, total: 0 })
  
  const [batchPoolsDialogOpen, setBatchPoolsDialogOpen] = useState(false)
  const [batchPoolsValue, setBatchPoolsValue] = useState<string[]>([])
  const [batchPoolsInput, setBatchPoolsInput] = useState('')
  const [batchUpdatingPools, setBatchUpdatingPools] = useState(false)

  const cancelVerifyRef = useRef(false)
  const [currentPage, setCurrentPage] = useState(1)
  const [searchTerm, setSearchTerm] = useState('')
  const [statusFilter, setStatusFilter] = useState<StatusFilter>('all')
  const [authFilter, setAuthFilter] = useState('all')
  const [profileFilter, setProfileFilter] = useState<ProfileFilter>('all')
  const [sortKey, setSortKey] = useState<CredentialSortKey>('priority')
  const [sortDirection, setSortDirection] = useState<SortDirection>('asc')
  const itemsPerPage = 12
  const [activeTab, setActiveTab] = useState<'credentials' | 'api-keys'>('credentials')
  const [darkMode, setDarkMode] = useState(() => {
    if (typeof window !== 'undefined') {
      return document.documentElement.classList.contains('dark')
    }
    return false
  })

  const queryClient = useQueryClient()
  const { data, isLoading, error, refetch } = useCredentials()
  const { mutate: deleteCredential } = useDeleteCredential()
  const { mutate: resetFailure } = useResetFailure()
  const { data: loadBalancingData, isLoading: isLoadingMode } = useLoadBalancingMode()
  const { mutate: setLoadBalancingMode, isPending: isSettingMode } = useSetLoadBalancingMode()
  const setCredentialPools = useSetCredentialPools()
  const { data: poolsData } = usePools()
  const availablePools = poolsData || []
  const isGrokAdmin = typeof window !== 'undefined' && (
    window.location.pathname === '/grok/admin'
    || window.location.pathname.startsWith('/grok/admin/')
  )

  const allCredentials = data?.credentials || []

  const authOptions = useMemo(() => {
    return Array.from(new Set(allCredentials.map(credential => credential.authMethod).filter((value): value is string => Boolean(value)))).sort()
  }, [allCredentials])

  const filteredCredentials = useMemo(() => {
    const query = searchTerm.trim().toLowerCase()

    return allCredentials.filter(credential => {
      if (query && !credentialSearchText(credential).includes(query)) {
        return false
      }

      if (statusFilter === 'enabled' && credential.disabled) return false
      if (statusFilter === 'disabled' && !credential.disabled) return false
      if (statusFilter === 'current' && !credential.isCurrent) return false
      if (statusFilter === 'failed' && credential.failureCount === 0 && credential.refreshFailureCount === 0) return false

      if (authFilter !== 'all' && credential.authMethod !== authFilter) {
        return false
      }

      if (profileFilter === 'has' && !credential.hasProfileArn) return false
      if (profileFilter === 'missing' && credential.hasProfileArn) return false

      return true
    })
  }, [allCredentials, authFilter, profileFilter, searchTerm, statusFilter])

  const sortedCredentials = useMemo(() => {
    const getValue = (credential: CredentialStatusItem): string | number => {
      switch (sortKey) {
        case 'name':
          return credential.name.toLowerCase()
        case 'id':
          return credential.id
        case 'priority':
          return credential.priority
        case 'status':
          return credential.disabled ? 2 : credential.isCurrent ? 0 : 1
        case 'authMethod':
          return credential.authMethod || ''
        case 'endpoint':
          return credential.endpoint || ''
        case 'profileArn':
          return credential.profileArn || ''
        case 'importedAt':
          return dateValue(credential.importedAt)
        case 'lastUsedAt':
          return dateValue(credential.lastUsedAt)
        case 'successCount':
          return credential.successCount
        case 'failureCount':
          return credential.failureCount + credential.refreshFailureCount
        case 'remaining':
          return balanceMap.get(credential.id)?.remaining ?? -1
      }
    }

    return [...filteredCredentials].sort((a, b) => {
      const aValue = getValue(a)
      const bValue = getValue(b)
      const direction = sortDirection === 'asc' ? 1 : -1

      if (typeof aValue === 'number' && typeof bValue === 'number') {
        return (aValue - bValue) * direction
      }

      return String(aValue).localeCompare(String(bValue), 'zh-CN') * direction
    })
  }, [balanceMap, filteredCredentials, sortDirection, sortKey])

  // 计算分页
  const totalPages = Math.ceil(sortedCredentials.length / itemsPerPage)
  const startIndex = (currentPage - 1) * itemsPerPage
  const endIndex = startIndex + itemsPerPage
  const currentCredentials = sortedCredentials.slice(startIndex, endIndex)
  const currentPageIds = currentCredentials.map(credential => credential.id)
  const allCurrentPageSelected = currentPageIds.length > 0 && currentPageIds.every(id => selectedIds.has(id))
  const disabledCredentialCount = allCredentials.filter(credential => credential.disabled).length || 0
  const selectedDisabledCount = Array.from(selectedIds).filter(id => {
    const credential = allCredentials.find(c => c.id === id)
    return Boolean(credential?.disabled)
  }).length

  // 当凭据列表或展示条件变化时重置到第一页
  useEffect(() => {
    setCurrentPage(1)
  }, [authFilter, data?.credentials.length, profileFilter, searchTerm, sortDirection, sortKey, statusFilter])

  // 只保留当前仍存在的凭据缓存，避免删除后残留旧数据
  useEffect(() => {
    if (!data?.credentials) {
      setBalanceMap(new Map())
      setLoadingBalanceIds(new Set())
      return
    }

    const validIds = new Set(data.credentials.map(credential => credential.id))

    setSelectedIds(prev => {
      if (prev.size === 0) {
        return prev
      }
      const next = new Set<number>()
      prev.forEach(id => {
        if (validIds.has(id)) {
          next.add(id)
        }
      })
      return next.size === prev.size ? prev : next
    })

    setBalanceMap(prev => {
      const next = new Map<number, BalanceResponse>()
      prev.forEach((value, id) => {
        if (validIds.has(id)) {
          next.set(id, value)
        }
      })
      return next.size === prev.size ? prev : next
    })

    setLoadingBalanceIds(prev => {
      if (prev.size === 0) {
        return prev
      }
      const next = new Set<number>()
      prev.forEach(id => {
        if (validIds.has(id)) {
          next.add(id)
        }
      })
      return next.size === prev.size ? prev : next
    })
  }, [data?.credentials])

  const toggleDarkMode = () => {
    setDarkMode(!darkMode)
    document.documentElement.classList.toggle('dark')
  }

  const handleViewBalance = (id: number) => {
    setSelectedCredentialId(id)
    setBalanceDialogOpen(true)
  }

  const handleViewCatalog = (id: number) => {
    setSelectedCredentialId(id)
    setCatalogDialogOpen(true)
  }

  const catalogCredentialLabel = useMemo(() => {
    if (selectedCredentialId == null) return undefined
    const c = allCredentials.find((item) => item.id === selectedCredentialId)
    if (!c) return `凭据 #${selectedCredentialId}`
    return c.name || c.userName || c.email || `凭据 #${c.id}`
  }, [allCredentials, selectedCredentialId])

  const handleRefresh = () => {
    refetch()
    toast.success('已刷新凭据列表')
  }

  const handleLogout = () => {
    storage.removeApiKey()
    queryClient.clear()
    onLogout()
  }

  // 选择管理
  const toggleSelect = (id: number) => {
    const newSelected = new Set(selectedIds)
    if (newSelected.has(id)) {
      newSelected.delete(id)
    } else {
      newSelected.add(id)
    }
    setSelectedIds(newSelected)
  }

  const toggleSelectCurrentPage = () => {
    setSelectedIds(prev => {
      const next = new Set(prev)
      if (allCurrentPageSelected) {
        currentPageIds.forEach(id => next.delete(id))
      } else {
        currentPageIds.forEach(id => next.add(id))
      }
      return next
    })
  }

  const deselectAll = () => {
    setSelectedIds(new Set())
  }

  const handleSort = (key: CredentialSortKey) => {
    if (sortKey === key) {
      setSortDirection(direction => direction === 'asc' ? 'desc' : 'asc')
      return
    }
    setSortKey(key)
    setSortDirection(key === 'importedAt' || key === 'lastUsedAt' ? 'desc' : 'asc')
  }

  // 批量删除（仅删除已禁用项）
  const handleBatchDelete = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要删除的凭据')
      return
    }

    const disabledIds = Array.from(selectedIds).filter(id => {
      const credential = allCredentials.find(c => c.id === id)
      return Boolean(credential?.disabled)
    })

    if (disabledIds.length === 0) {
      toast.error('选中的凭据中没有已禁用项')
      return
    }

    const skippedCount = selectedIds.size - disabledIds.length
    const skippedText = skippedCount > 0 ? `（将跳过 ${skippedCount} 个未禁用凭据）` : ''

    if (!confirm(`确定要删除 ${disabledIds.length} 个已禁用凭据吗？此操作无法撤销。${skippedText}`)) {
      return
    }

    let successCount = 0
    let failCount = 0

    for (const id of disabledIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            }
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    const skippedResultText = skippedCount > 0 ? `，已跳过 ${skippedCount} 个未禁用凭据` : ''

    if (failCount === 0) {
      toast.success(`成功删除 ${successCount} 个已禁用凭据${skippedResultText}`)
    } else {
      toast.warning(`删除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个${skippedResultText}`)
    }

    deselectAll()
  }

  // 批量恢复异常
  const handleBatchResetFailure = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要恢复的凭据')
      return
    }

    const failedIds = Array.from(selectedIds).filter(id => {
      const cred = allCredentials.find(c => c.id === id)
      return cred && cred.failureCount > 0
    })

    if (failedIds.length === 0) {
      toast.error('选中的凭据中没有失败的凭据')
      return
    }

    let successCount = 0
    let failCount = 0

    for (const id of failedIds) {
      try {
        await new Promise<void>((resolve, reject) => {
          resetFailure(id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            }
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    if (failCount === 0) {
      toast.success(`成功恢复 ${successCount} 个凭据`)
    } else {
      toast.warning(`成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 批量修改资源池
  const handleBatchUpdatePools = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要修改的凭据')
      return
    }

    setBatchUpdatingPools(true)
    let successCount = 0
    let failCount = 0

    const ids = Array.from(selectedIds)
    for (const id of ids) {
      try {
        await new Promise<void>((resolve, reject) => {
          setCredentialPools.mutate(
            { id, pools: batchPoolsValue },
            {
              onSuccess: () => {
                successCount++
                resolve()
              },
              onError: (err) => {
                failCount++
                reject(err)
              }
            }
          )
        })
      } catch (error) {
        // 错误已处理
      }
    }

    setBatchUpdatingPools(false)
    setBatchPoolsDialogOpen(false)
    setBatchPoolsValue([])
    
    if (failCount === 0) {
      toast.success(`成功更新 ${successCount} 个凭据的资源池`)
    } else {
      toast.warning(`更新凭据资源池：成功 ${successCount} 个，失败 ${failCount} 个`)
    }
    
    deselectAll()
  }

  // 批量刷新 Token
  const handleBatchForceRefresh = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要刷新的凭据')
      return
    }

    const enabledIds = Array.from(selectedIds).filter(id => {
      const cred = allCredentials.find(c => c.id === id)
      return cred && !cred.disabled
    })

    if (enabledIds.length === 0) {
      toast.error('选中的凭据中没有启用的凭据')
      return
    }

    setBatchRefreshing(true)
    setBatchRefreshProgress({ current: 0, total: enabledIds.length })

    let successCount = 0
    let failCount = 0

    for (let i = 0; i < enabledIds.length; i++) {
      try {
        await forceRefreshToken(enabledIds[i])
        successCount++
      } catch {
        failCount++
      }
      setBatchRefreshProgress({ current: i + 1, total: enabledIds.length })
    }

    setBatchRefreshing(false)
    queryClient.invalidateQueries({ queryKey: ['credentials'] })

    if (failCount === 0) {
      toast.success(`成功刷新 ${successCount} 个凭据的 Token`)
    } else {
      toast.warning(`刷新 Token：成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 一键清除所有已禁用凭据
  const handleClearAll = async () => {
    if (allCredentials.length === 0) {
      toast.error('没有可清除的凭据')
      return
    }

    const disabledCredentials = allCredentials.filter(credential => credential.disabled)

    if (disabledCredentials.length === 0) {
      toast.error('没有可清除的已禁用凭据')
      return
    }

    if (!confirm(`确定要清除所有 ${disabledCredentials.length} 个已禁用凭据吗？此操作无法撤销。`)) {
      return
    }

    let successCount = 0
    let failCount = 0

    for (const credential of disabledCredentials) {
      try {
        await new Promise<void>((resolve, reject) => {
          deleteCredential(credential.id, {
            onSuccess: () => {
              successCount++
              resolve()
            },
            onError: (err) => {
              failCount++
              reject(err)
            }
          })
        })
      } catch (error) {
        // 错误已在 onError 中处理
      }
    }

    if (failCount === 0) {
      toast.success(`成功清除所有 ${successCount} 个已禁用凭据`)
    } else {
      toast.warning(`清除已禁用凭据：成功 ${successCount} 个，失败 ${failCount} 个`)
    }

    deselectAll()
  }

  // 查询当前页凭据信息（逐个查询，避免瞬时并发）
  const handleQueryCurrentPageInfo = async () => {
    if (currentCredentials.length === 0) {
      toast.error('当前页没有可查询的凭据')
      return
    }

    const ids = currentCredentials
      .filter(credential => !credential.disabled)
      .map(credential => credential.id)

    if (ids.length === 0) {
      toast.error('当前页没有可查询的启用凭据')
      return
    }

    setQueryingInfo(true)
    setQueryInfoProgress({ current: 0, total: ids.length })

    let successCount = 0
    let failCount = 0

    for (let i = 0; i < ids.length; i++) {
      const id = ids[i]

      setLoadingBalanceIds(prev => {
        const next = new Set(prev)
        next.add(id)
        return next
      })

      try {
        const balance = await getCredentialBalance(id)
        successCount++

        setBalanceMap(prev => {
          const next = new Map(prev)
          next.set(id, balance)
          return next
        })
      } catch (error) {
        failCount++
      } finally {
        setLoadingBalanceIds(prev => {
          const next = new Set(prev)
          next.delete(id)
          return next
        })
      }

      setQueryInfoProgress({ current: i + 1, total: ids.length })
    }

    setQueryingInfo(false)

    if (failCount === 0) {
      toast.success(`查询完成：成功 ${successCount}/${ids.length}`)
    } else {
      toast.warning(`查询完成：成功 ${successCount} 个，失败 ${failCount} 个`)
    }
  }

  // 批量验活
  const handleBatchVerify = async () => {
    if (selectedIds.size === 0) {
      toast.error('请先选择要验活的凭据')
      return
    }

    // 初始化状态
    setVerifying(true)
    cancelVerifyRef.current = false
    const ids = Array.from(selectedIds)
    setVerifyProgress({ current: 0, total: ids.length })

    let successCount = 0

    // 初始化结果，所有凭据状态为 pending
    const initialResults = new Map<number, VerifyResult>()
    ids.forEach(id => {
      initialResults.set(id, { id, status: 'pending' })
    })
    setVerifyResults(initialResults)
    setVerifyDialogOpen(true)

    // 开始验活
    for (let i = 0; i < ids.length; i++) {
      // 检查是否取消
      if (cancelVerifyRef.current) {
        toast.info('已取消验活')
        break
      }

      const id = ids[i]

      // 更新当前凭据状态为 verifying
      setVerifyResults(prev => {
        const newResults = new Map(prev)
        newResults.set(id, { id, status: 'verifying' })
        return newResults
      })

      try {
        const balance = await getCredentialBalance(id)
        successCount++

        // 更新为成功状态
        setVerifyResults(prev => {
          const newResults = new Map(prev)
          newResults.set(id, {
            id,
            status: 'success',
            usage: `${balance.currentUsage}/${balance.usageLimit}`
          })
          return newResults
        })
      } catch (error) {
        // 更新为失败状态
        setVerifyResults(prev => {
          const newResults = new Map(prev)
          newResults.set(id, {
            id,
            status: 'failed',
            error: extractErrorMessage(error)
          })
          return newResults
        })
      }

      // 更新进度
      setVerifyProgress({ current: i + 1, total: ids.length })

      // 添加延迟防止封号（最后一个不需要延迟）
      if (i < ids.length - 1 && !cancelVerifyRef.current) {
        await new Promise(resolve => setTimeout(resolve, 2000))
      }
    }

    setVerifying(false)

    if (!cancelVerifyRef.current) {
      toast.success(`验活完成：成功 ${successCount}/${ids.length}`)
    }
  }

  // 取消验活
  const handleCancelVerify = () => {
    cancelVerifyRef.current = true
    setVerifying(false)
  }

  // 切换负载均衡模式
  const handleToggleLoadBalancing = () => {
    const currentMode = loadBalancingData?.mode || 'round_robin'
    const newMode = nextLoadBalancingMode(currentMode)

    setLoadBalancingMode(newMode, {
      onSuccess: () => {
        const modeName = LOAD_BALANCING_LABELS[newMode]
        toast.success(`已切换到${modeName}`)
      },
      onError: (error) => {
        toast.error(`切换失败: ${extractErrorMessage(error)}`)
      }
    })
  }

  if (isLoading) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-background">
        <div className="text-center">
          <div className="animate-spin rounded-full h-12 w-12 border-b-2 border-primary mx-auto mb-4"></div>
          <p className="text-muted-foreground">加载中...</p>
        </div>
      </div>
    )
  }

  if (error) {
    return (
      <div className="min-h-screen flex items-center justify-center bg-background p-4">
        <Card className="w-full max-w-md">
          <CardContent className="pt-6 text-center">
            <div className="text-red-500 mb-4">加载失败</div>
            <p className="text-muted-foreground mb-4">{(error as Error).message}</p>
            <div className="space-x-2">
              <Button onClick={() => refetch()}>重试</Button>
              <Button variant="outline" onClick={handleLogout}>重新登录</Button>
            </div>
          </CardContent>
        </Card>
      </div>
    )
  }

  return (
    <div className="min-h-screen bg-background">
      {/* 顶部导航 */}
      <header className="sticky top-0 z-50 w-full border-b bg-background/95 backdrop-blur supports-[backdrop-filter]:bg-background/60">
        <div className="container flex h-14 items-center justify-between px-4 md:px-8">
          <div className="flex items-center gap-2">
            <Server className="h-5 w-5" />
            <span className="font-semibold">Kiro Admin</span>
          </div>
          <div className="flex items-center gap-2">
            <Button
              variant="outline"
              size="sm"
              onClick={handleToggleLoadBalancing}
              disabled={isLoadingMode || isSettingMode}
              title="切换负载均衡模式：轮询模式 / 优先级模式 / 均衡负载"
            >
              {isLoadingMode ? '加载中...' : LOAD_BALANCING_LABELS[loadBalancingData?.mode || 'round_robin']}
            </Button>
            <Button variant="ghost" size="icon" onClick={toggleDarkMode}>
              {darkMode ? <Sun className="h-5 w-5" /> : <Moon className="h-5 w-5" />}
            </Button>
            <Button variant="ghost" size="icon" onClick={handleRefresh}>
              <RefreshCw className="h-5 w-5" />
            </Button>
            <Button variant="ghost" size="icon" onClick={handleLogout}>
              <LogOut className="h-5 w-5" />
            </Button>
          </div>
        </div>
      </header>

      {/* 主内容 */}
      <main className="container mx-auto px-4 md:px-8 py-6">
        {/* 统计卡片 */}
        <div className="grid gap-4 md:grid-cols-3 mb-6">
          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-sm font-medium text-muted-foreground">
                凭据总数
              </CardTitle>
            </CardHeader>
            <CardContent>
              <div className="text-2xl font-bold">{data?.total || 0}</div>
            </CardContent>
          </Card>
          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-sm font-medium text-muted-foreground">
                可用凭据
              </CardTitle>
            </CardHeader>
            <CardContent>
              <div className="text-2xl font-bold text-green-600">{data?.available || 0}</div>
            </CardContent>
          </Card>
          <Card>
            <CardHeader className="pb-2">
              <CardTitle className="text-sm font-medium text-muted-foreground">
                当前活跃
              </CardTitle>
            </CardHeader>
            <CardContent>
              <div className="text-2xl font-bold flex items-center gap-2">
                #{data?.currentId || '-'}
                <Badge variant="success">活跃</Badge>
              </div>
            </CardContent>
          </Card>
        </div>

        {/* 导航 Tabs */}
        <div className="flex gap-4 border-b mb-6">
          <button
            className={`pb-2 text-lg font-semibold transition-colors ${activeTab === 'credentials' ? 'border-b-2 border-primary text-foreground' : 'text-muted-foreground hover:text-foreground'}`}
            onClick={() => setActiveTab('credentials')}
          >
            后端凭据 (Credentials)
          </button>
          <button
            className={`pb-2 text-lg font-semibold transition-colors ${activeTab === 'api-keys' ? 'border-b-2 border-primary text-foreground' : 'text-muted-foreground hover:text-foreground'}`}
            onClick={() => setActiveTab('api-keys')}
          >
            前端密钥 (API Keys)
          </button>
        </div>

        {/* 凭据列表 */}
        {activeTab === 'credentials' ? (
          <div className="space-y-4">
            <div className="flex flex-col gap-3 lg:flex-row lg:items-center lg:justify-between">
            <div className="flex items-center gap-4">
              <h2 className="text-xl font-semibold">凭据管理</h2>
              {selectedIds.size > 0 && (
                <div className="flex items-center gap-2">
                  <Badge variant="secondary">已选择 {selectedIds.size} 个</Badge>
                  <Button onClick={deselectAll} size="sm" variant="ghost">
                    取消选择
                  </Button>
                </div>
              )}
            </div>
            <div className="flex flex-wrap gap-2">
              {selectedIds.size > 0 && (
                <>
                  <Button onClick={handleBatchVerify} size="sm" variant="outline">
                    <CheckCircle2 className="h-4 w-4 mr-2" />
                    批量验活
                  </Button>
                  <Button
                    onClick={handleBatchForceRefresh}
                    size="sm"
                    variant="outline"
                    disabled={batchRefreshing}
                  >
                    <RefreshCw className={`h-4 w-4 mr-2 ${batchRefreshing ? 'animate-spin' : ''}`} />
                    {batchRefreshing ? `刷新中... ${batchRefreshProgress.current}/${batchRefreshProgress.total}` : '批量刷新 Token'}
                  </Button>
                  <Button onClick={handleBatchResetFailure} size="sm" variant="outline">
                    <RotateCcw className="h-4 w-4 mr-2" />
                    恢复异常
                  </Button>
                  <Button onClick={() => setBatchPoolsDialogOpen(true)} size="sm" variant="outline">
                    <Tags className="h-4 w-4 mr-2" />
                    批量修改池
                  </Button>
                  <Button
                    onClick={handleBatchDelete}
                    size="sm"
                    variant="destructive"
                    disabled={selectedDisabledCount === 0}
                    title={selectedDisabledCount === 0 ? '只能删除已禁用凭据' : undefined}
                  >
                    <Trash2 className="h-4 w-4 mr-2" />
                    批量删除
                  </Button>
                </>
              )}
              {verifying && !verifyDialogOpen && (
                <Button onClick={() => setVerifyDialogOpen(true)} size="sm" variant="secondary">
                  <CheckCircle2 className="h-4 w-4 mr-2 animate-spin" />
                  验活中... {verifyProgress.current}/{verifyProgress.total}
                </Button>
              )}
              {allCredentials.length > 0 && (
                <Button
                  onClick={handleQueryCurrentPageInfo}
                  size="sm"
                  variant="outline"
                  disabled={queryingInfo}
                >
                  <RefreshCw className={`h-4 w-4 mr-2 ${queryingInfo ? 'animate-spin' : ''}`} />
                  {queryingInfo ? `查询中... ${queryInfoProgress.current}/${queryInfoProgress.total}` : '查询信息'}
                </Button>
              )}
              {allCredentials.length > 0 && (
                <Button
                  onClick={handleClearAll}
                  size="sm"
                  variant="outline"
                  className="text-destructive hover:text-destructive"
                  disabled={disabledCredentialCount === 0}
                  title={disabledCredentialCount === 0 ? '没有可清除的已禁用凭据' : undefined}
                >
                  <Trash2 className="h-4 w-4 mr-2" />
                  清除已禁用
                </Button>
              )}
              {isGrokAdmin ? (
                <Button onClick={() => setGrokOauthDialogOpen(true)} size="sm" variant="outline">
                  <ShieldCheck className="h-4 w-4 mr-2" />
                  Grok OAuth 授权
                </Button>
              ) : (
                <>
                  <Button onClick={() => setKamImportDialogOpen(true)} size="sm" variant="outline">
                    <FileUp className="h-4 w-4 mr-2" />
                    Kiro Account Manager 导入
                  </Button>
                  <Button onClick={() => setBatchImportDialogOpen(true)} size="sm" variant="outline">
                    <Upload className="h-4 w-4 mr-2" />
                    批量导入
                  </Button>
                </>
              )}
              <Button onClick={() => setAddDialogOpen(true)} size="sm">
                <Plus className="h-4 w-4 mr-2" />
                {isGrokAdmin ? '添加 xAI Token' : '添加凭据'}
              </Button>
            </div>
          </div>

          {allCredentials.length > 0 && (
            <div className="grid gap-3 md:grid-cols-[minmax(240px,1fr)_150px_150px_150px]">
              <Input
                value={searchTerm}
                onChange={(event) => setSearchTerm(event.target.value)}
                placeholder="搜索名称、邮箱、用户、ARN、端点"
                className="h-9"
              />
              <select
                value={statusFilter}
                onChange={(event) => setStatusFilter(event.target.value as StatusFilter)}
                className="h-9 rounded-md border border-input bg-background px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <option value="all">全部状态</option>
                <option value="enabled">仅启用</option>
                <option value="disabled">仅禁用</option>
                <option value="current">当前活跃</option>
                <option value="failed">有异常</option>
              </select>
              <select
                value={authFilter}
                onChange={(event) => setAuthFilter(event.target.value)}
                className="h-9 rounded-md border border-input bg-background px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <option value="all">全部认证</option>
                {authOptions.map(method => (
                  <option key={method} value={method}>{method === 'api_key' ? 'API Key' : method}</option>
                ))}
              </select>
              <select
                value={profileFilter}
                onChange={(event) => setProfileFilter(event.target.value as ProfileFilter)}
                className="h-9 rounded-md border border-input bg-background px-3 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
              >
                <option value="all">全部 ARN</option>
                <option value="has">已有 ARN</option>
                <option value="missing">缺失 ARN</option>
              </select>
            </div>
          )}

          {allCredentials.length === 0 ? (
            <Card>
              <CardContent className="py-8 text-center text-muted-foreground">
                暂无凭据
              </CardContent>
            </Card>
          ) : sortedCredentials.length === 0 ? (
            <Card>
              <CardContent className="py-8 text-center text-muted-foreground">
                没有匹配当前过滤条件的凭据
              </CardContent>
            </Card>
          ) : (
            <>
              <CredentialsTable
                credentials={currentCredentials}
                selectedIds={selectedIds}
                allSelected={allCurrentPageSelected}
                onToggleSelect={toggleSelect}
                onToggleSelectAll={toggleSelectCurrentPage}
                onViewBalance={handleViewBalance}
                onViewCatalog={handleViewCatalog}
                balanceMap={balanceMap}
                loadingBalanceIds={loadingBalanceIds}
                sortKey={sortKey}
                sortDirection={sortDirection}
                onSort={handleSort}
              />

              {/* 分页控件 */}
              {totalPages > 1 && (
                <div className="flex justify-center items-center gap-4 mt-6">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setCurrentPage(p => Math.max(1, p - 1))}
                    disabled={currentPage === 1}
                  >
                    上一页
                  </Button>
                  <span className="text-sm text-muted-foreground">
                    第 {currentPage} / {totalPages} 页（共 {sortedCredentials.length} 个，全部 {allCredentials.length} 个）
                  </span>
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => setCurrentPage(p => Math.min(totalPages, p + 1))}
                    disabled={currentPage === totalPages}
                  >
                    下一页
                  </Button>
                </div>
              )}
            </>
          )}
        </div>
        ) : (
          <ApiKeysManager />
        )}
      </main>

      {/* 余额对话框 */}
      <BalanceDialog
        credentialId={selectedCredentialId}
        open={balanceDialogOpen}
        onOpenChange={setBalanceDialogOpen}
      />

      {/* 模型目录对话框 */}
      <CatalogDialog
        credentialId={selectedCredentialId}
        credentialLabel={catalogCredentialLabel}
        open={catalogDialogOpen}
        onOpenChange={setCatalogDialogOpen}
      />

      {/* 添加凭据对话框 */}
      <AddCredentialDialog
        open={addDialogOpen}
        onOpenChange={setAddDialogOpen}
      />

      <GrokOAuthDialog
        open={grokOauthDialogOpen}
        onOpenChange={setGrokOauthDialogOpen}
      />

      {/* 批量导入对话框 */}
      <BatchImportDialog
        open={batchImportDialogOpen}
        onOpenChange={setBatchImportDialogOpen}
      />

      {/* KAM 账号导入对话框 */}
      <KamImportDialog
        open={kamImportDialogOpen}
        onOpenChange={setKamImportDialogOpen}
      />

      {/* 批量验活对话框 */}
      <BatchVerifyDialog
        open={verifyDialogOpen}
        onOpenChange={setVerifyDialogOpen}
        verifying={verifying}
        progress={verifyProgress}
        results={verifyResults}
        onCancel={handleCancelVerify}
      />

      {/* 批量修改资源池对话框 */}
      <Dialog open={batchPoolsDialogOpen} onOpenChange={(open: boolean) => {
        if (!open) {
          setBatchPoolsValue([])
          setBatchPoolsInput('')
        }
        setBatchPoolsDialogOpen(open)
      }}>
        <DialogContent>
          <DialogHeader>
            <DialogTitle>批量修改权限池</DialogTitle>
          </DialogHeader>
          <div className="space-y-4 py-2">
            <p className="text-sm text-muted-foreground">
              修改选中的 <strong>{selectedIds.size}</strong> 个凭据所属的资源池。这会<strong>替换</strong>它们原有的资源池。
            </p>
            
            <div className="space-y-3">
              <label className="text-sm font-medium">选择权限池 (Pools)</label>
              {(() => {
                const allOptions = Array.from(new Set([...availablePools, ...batchPoolsValue])).filter(Boolean)
                if (allOptions.length === 0) {
                  return <p className="text-xs text-muted-foreground">暂无可用的凭据池，请在下方新增</p>
                }
                return (
                  <div className="grid grid-cols-2 gap-2 p-3 border rounded-md bg-muted/20 max-h-[160px] overflow-y-auto">
                    {allOptions.map(pool => {
                      const isChecked = batchPoolsValue.includes(pool)
                      return (
                        <div key={pool} className="flex items-center space-x-2">
                          <Checkbox
                            id={`batch-pool-${pool}`}
                            checked={isChecked}
                            onCheckedChange={() => {
                              if (isChecked) {
                                setBatchPoolsValue(prev => prev.filter(p => p !== pool))
                              } else {
                                setBatchPoolsValue(prev => [...prev, pool])
                              }
                            }}
                          />
                          <label
                            htmlFor={`batch-pool-${pool}`}
                            className="text-sm font-medium leading-none cursor-pointer select-none truncate"
                            title={pool}
                          >
                            {pool}
                          </label>
                        </div>
                      )
                    })}
                  </div>
                )
              })()}

              <div className="flex gap-2">
                <Input
                  value={batchPoolsInput}
                  onChange={e => setBatchPoolsInput(e.target.value)}
                  placeholder="输入新资源池名称"
                  className="h-9"
                  onKeyDown={e => {
                    if (e.key === 'Enter') {
                      e.preventDefault()
                      const pool = batchPoolsInput.trim()
                      if (pool && !batchPoolsValue.includes(pool)) {
                        setBatchPoolsValue(prev => [...prev, pool])
                      }
                      setBatchPoolsInput('')
                    }
                  }}
                />
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  onClick={() => {
                    const pool = batchPoolsInput.trim()
                    if (pool && !batchPoolsValue.includes(pool)) {
                      setBatchPoolsValue(prev => [...prev, pool])
                    }
                    setBatchPoolsInput('')
                  }}
                  className="h-9 whitespace-nowrap"
                >
                  添加新池
                </Button>
              </div>
            </div>
          </div>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setBatchPoolsDialogOpen(false)}
              disabled={batchUpdatingPools}
            >
              取消
            </Button>
            <Button
              onClick={handleBatchUpdatePools}
              disabled={batchUpdatingPools}
            >
              {batchUpdatingPools ? '正在更新...' : '确认修改'}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  )
}
