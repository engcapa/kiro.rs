import { useEffect, useState } from 'react'
import { ExternalLink, Loader2 } from 'lucide-react'
import { useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  cancelGrokOAuth,
  getGrokOAuthStatus,
  startGrokOAuth,
} from '@/api/credentials'
import type { GrokOAuthStartResponse, GrokOAuthStatus } from '@/types/api'
import { extractErrorMessage } from '@/lib/utils'
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'

interface GrokOAuthDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

const STATUS_TEXT: Record<GrokOAuthStatus, string> = {
  pending: '等待在 xAI 页面完成授权…',
  completed: '授权成功，凭据已保存。',
  failed: '授权失败。',
  cancelled: '授权已取消。',
}

export function GrokOAuthDialog({ open, onOpenChange }: GrokOAuthDialogProps) {
  const queryClient = useQueryClient()
  const [flow, setFlow] = useState<GrokOAuthStartResponse | null>(null)
  const [status, setStatus] = useState<GrokOAuthStatus | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [starting, setStarting] = useState(false)
  const [cancelling, setCancelling] = useState(false)

  useEffect(() => {
    if (!flow || status !== 'pending') return

    let alive = true
    const poll = async () => {
      try {
        const result = await getGrokOAuthStatus(flow.state)
        if (!alive) return
        setStatus(result.status)
        setError(result.error || null)
        if (result.status === 'completed') {
          await queryClient.invalidateQueries({ queryKey: ['credentials'] })
          if (alive) {
            toast.success(result.email ? `Grok OAuth 授权成功：${result.email}` : 'Grok OAuth 授权成功')
          }
        }
      } catch (requestError) {
        if (alive) {
          setStatus('failed')
          setError(extractErrorMessage(requestError))
        }
      }
    }

    void poll()
    const intervalId = window.setInterval(() => void poll(), 2000)
    return () => {
      alive = false
      window.clearInterval(intervalId)
    }
  }, [flow, queryClient, status])

  const begin = async () => {
    setStarting(true)
    setError(null)
    try {
      const nextFlow = await startGrokOAuth()
      setFlow(nextFlow)
      setStatus('pending')
      const popup = window.open(nextFlow.authorizationUrl, 'grok-oauth', 'popup,width=640,height=760')
      if (!popup) {
        toast.message('浏览器拦截了弹窗，请使用下方“打开 xAI 授权页”链接继续。')
      }
    } catch (requestError) {
      setError(extractErrorMessage(requestError))
      toast.error(`无法启动 Grok OAuth：${extractErrorMessage(requestError)}`)
    } finally {
      setStarting(false)
    }
  }

  const cancel = async () => {
    if (!flow || status !== 'pending') return
    setCancelling(true)
    try {
      await cancelGrokOAuth(flow.state)
      setStatus('cancelled')
    } catch (requestError) {
      toast.error(`取消授权失败：${extractErrorMessage(requestError)}`)
    } finally {
      setCancelling(false)
    }
  }

  const handleOpenChange = (nextOpen: boolean) => {
    if (!nextOpen) {
      void cancel()
      setFlow(null)
      setStatus(null)
      setError(null)
    }
    onOpenChange(nextOpen)
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Grok Build OAuth 授权</DialogTitle>
        </DialogHeader>

        {!flow ? (
          <div className="space-y-3 text-sm text-muted-foreground">
            <p>将使用 xAI Grok CLI 的 OAuth + PKCE 流程，授权成功后会自动写入 Grok 凭据池并自动刷新 Token。</p>
            <p>授权浏览器需要和服务运行在同一台主机（回调地址为 <code>127.0.0.1:56121</code>）。</p>
            {error && <p className="text-destructive">{error}</p>}
          </div>
        ) : (
          <div className="space-y-3 text-sm">
            <p className="text-muted-foreground">{status ? STATUS_TEXT[status] : '准备授权…'}</p>
            {status === 'pending' && (
              <a
                className="inline-flex items-center gap-1 text-primary underline underline-offset-4"
                href={flow.authorizationUrl}
                target="_blank"
                rel="noreferrer"
              >
                打开 xAI 授权页 <ExternalLink className="h-3.5 w-3.5" />
              </a>
            )}
            {error && <p className="text-destructive break-words">{error}</p>}
          </div>
        )}

        <DialogFooter>
          {flow && status === 'pending' && (
            <Button variant="outline" onClick={() => void cancel()} disabled={cancelling}>
              {cancelling ? '取消中…' : '取消授权'}
            </Button>
          )}
          {!flow ? (
            <Button onClick={() => void begin()} disabled={starting}>
              {starting && <Loader2 className="mr-2 h-4 w-4 animate-spin" />}
              {starting ? '正在创建授权…' : '开始授权'}
            </Button>
          ) : status !== 'pending' ? (
            <Button onClick={() => handleOpenChange(false)}>关闭</Button>
          ) : null}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
