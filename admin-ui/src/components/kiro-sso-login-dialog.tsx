import { useCallback, useEffect, useRef, useState } from 'react'
import { toast } from 'sonner'
import { useQueryClient } from '@tanstack/react-query'
import { Copy, Check } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  startKiroSso,
  startKiroIdc,
  submitKiroIdcCallback,
  pollKiroSso,
  cancelKiroSso,
} from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

interface KiroSsoLoginDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/**
 * SSH 端口转发提示框。
 *
 * kiro-rs 部署在云服务器时，SSO 回调会被浏览器强制重定向到写死的 http://localhost:3128，
 * 该 localhost 指向用户本机而非服务器。因此需要在“运行浏览器的本机”建立 SSH 隧道，把本机
 * localhost:3128 转发到服务器的 127.0.0.1:3128（回环监听器所在），回调才能到达服务端。
 *
 * 命令中的服务器地址自动填为当前访问后台的 hostname，用户只需替换用户名。
 */
function SshTunnelHint() {
  const [copied, setCopied] = useState(false)
  // 当前访问后台的主机名即服务器地址（用户从浏览器访问的正是它）
  const host =
    typeof window !== 'undefined' && window.location.hostname
      ? window.location.hostname
      : '云服务器IP'
  const cmd = `ssh -N -L 3128:127.0.0.1:3128 你的用户@${host}`

  const copy = useCallback(() => {
    navigator.clipboard
      .writeText(cmd)
      .then(() => {
        setCopied(true)
        setTimeout(() => setCopied(false), 1500)
      })
      .catch(() => toast.error('复制失败，请手动选择命令复制'))
  }, [cmd])

  return (
    <div className="rounded-md border border-amber-500/40 bg-amber-50 dark:bg-amber-950/30 p-3 space-y-2">
      <p className="text-xs font-medium text-amber-700 dark:text-amber-400">
        ⚠️ 云服务器部署：先在“运行浏览器的本机”建立 SSH 隧道
      </p>
      <p className="text-xs text-muted-foreground">
        SSO 回调会跳转到本机的 <code>localhost:3128</code>，需转发到服务器。
        在本地终端运行以下命令（替换用户名），保持不关，再点“开始登录”：
      </p>
      <div className="flex items-center gap-2">
        <code className="flex-1 text-xs bg-muted rounded px-2 py-1.5 break-all font-mono">
          {cmd}
        </code>
        <Button
          type="button"
          size="sm"
          variant="outline"
          className="shrink-0 h-8 px-2"
          onClick={copy}
        >
          {copied ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
        </Button>
      </div>
      <p className="text-xs text-muted-foreground">
        提示：登录须在 10 分钟内完成；必须用开隧道那台机器的浏览器；确保本机 3128 端口未被占用。
      </p>
    </div>
  )
}

type Phase = 'idle' | 'starting' | 'waiting' | 'completed' | 'error'

/**
 * Kiro SSO（企业 Azure 租户）浏览器登录对话框。
 *
 * 流程：start 拿到 signInUrl + sessionId → 打开浏览器让用户在企业 IdP 完成登录 →
 * 轮询 poll 直到 completed（授权码已被回环监听捕获并落库）。取消 / 关闭时调用 cancel
 * 拆除后台会话，避免回环端口泄漏。
 */
export function KiroSsoLoginDialog({ open, onOpenChange }: KiroSsoLoginDialogProps) {
  const queryClient = useQueryClient()
  const [mode, setMode] = useState<'social' | 'idc'>('social')
  const [region, setRegion] = useState('')
  const [startUrl, setStartUrl] = useState('')
  const [phase, setPhase] = useState<Phase>('idle')
  const [signInUrl, setSignInUrl] = useState('')
  const [errorMsg, setErrorMsg] = useState('')
  // 手动回调（无 SSH 隧道场景）：用户粘贴浏览器地址栏的完整回调 URL
  const [manualCallback, setManualCallback] = useState('')
  const [submittingCallback, setSubmittingCallback] = useState(false)

  const sessionIdRef = useRef<string | null>(null)
  const pollTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const cancelledRef = useRef(false)

  const clearPollTimer = useCallback(() => {
    if (pollTimerRef.current) {
      clearTimeout(pollTimerRef.current)
      pollTimerRef.current = null
    }
  }, [])

  // 拆除后台会话（fire-and-forget）
  const teardownSession = useCallback(() => {
    clearPollTimer()
    const sid = sessionIdRef.current
    sessionIdRef.current = null
    if (sid) {
      cancelKiroSso(sid).catch(() => {
        /* 忽略：会话可能已在服务端结束 */
      })
    }
  }, [clearPollTimer])

  const resetState = useCallback(() => {
    setMode('social')
    setRegion('')
    setStartUrl('')
    setPhase('idle')
    setSignInUrl('')
    setErrorMsg('')
    setManualCallback('')
    setSubmittingCallback(false)
  }, [])

  // 轮询循环：completed=true 表示登录成功；服务端错误（HTTP 4xx/5xx）视为失败
  const schedulePoll = useCallback(
    (sessionId: string, intervalMs: number) => {
      clearPollTimer()
      pollTimerRef.current = setTimeout(async () => {
        if (cancelledRef.current || sessionIdRef.current !== sessionId) return
        try {
          const res = await pollKiroSso(sessionId)
          if (cancelledRef.current || sessionIdRef.current !== sessionId) return
          if (res.completed) {
            sessionIdRef.current = null
            setPhase('completed')
            const label = res.email ? `（${res.email}）` : ''
            toast.success(`Kiro SSO 登录成功${label}`)
            queryClient.invalidateQueries({ queryKey: ['credentials'] })
            queryClient.invalidateQueries({ queryKey: ['cached-balances'] })
            return
          }
          // 仍在等待，继续轮询
          schedulePoll(sessionId, intervalMs)
        } catch (error: unknown) {
          if (cancelledRef.current || sessionIdRef.current !== sessionId) return
          sessionIdRef.current = null
          const msg = extractErrorMessage(error)
          setErrorMsg(msg)
          setPhase('error')
          toast.error(`登录失败: ${msg}`)
        }
      }, intervalMs)
    },
    [clearPollTimer, queryClient]
  )

  const handleStart = useCallback(async () => {
    if (mode === 'idc' && !startUrl.trim()) {
      toast.error('请填写 IAM Identity Center 的 Start URL')
      return
    }
    cancelledRef.current = false
    setErrorMsg('')
    setPhase('starting')
    try {
      const res =
        mode === 'idc'
          ? await startKiroIdc({
              startUrl: startUrl.trim(),
              region: region.trim() || undefined,
            })
          : await startKiroSso({ region: region.trim() || undefined })
      sessionIdRef.current = res.sessionId
      setSignInUrl(res.signInUrl)
      setPhase('waiting')
      // 自动打开浏览器（弹窗被拦截时用户可点击下方链接）
      window.open(res.signInUrl, '_blank', 'noopener,noreferrer')
      const intervalMs = Math.max(1, res.interval || 2) * 1000
      schedulePoll(res.sessionId, intervalMs)
    } catch (error: unknown) {
      const msg = extractErrorMessage(error)
      setErrorMsg(msg)
      setPhase('error')
      toast.error(`启动登录失败: ${msg}`)
    }
  }, [mode, region, startUrl, schedulePoll])

  // 手动提交回调 URL（无 SSH 隧道场景）。提交成功后不需自己 poll——
  // 后台 poll 循环会检测到已投递的授权码并完成落库。
  const handleSubmitCallback = useCallback(async () => {
    const sid = sessionIdRef.current
    if (!sid) {
      toast.error('会话已失效，请重新发起登录')
      return
    }
    if (!manualCallback.trim()) {
      toast.error('请粘贴完整的回调地址')
      return
    }
    setSubmittingCallback(true)
    try {
      await submitKiroIdcCallback(sid, manualCallback.trim())
      toast.success('回调已提交，正在换取 token…')
      // poll 循环仍在跑，completed 后会自动切到 completed 状态
    } catch (error: unknown) {
      const msg = extractErrorMessage(error)
      toast.error(`提交回调失败: ${msg}`)
    } finally {
      setSubmittingCallback(false)
    }
  }, [manualCallback])

  const handleClose = useCallback(() => {
    cancelledRef.current = true
    teardownSession()
    resetState()
    onOpenChange(false)
  }, [teardownSession, resetState, onOpenChange])

  // 卸载时确保清理定时器 + 会话
  useEffect(() => {
    return () => {
      cancelledRef.current = true
      teardownSession()
    }
  }, [teardownSession])

  // 对话框打开时重置为初始状态
  useEffect(() => {
    if (open) {
      cancelledRef.current = false
      resetState()
    }
  }, [open, resetState])

  const isBusy = phase === 'starting' || phase === 'waiting'

  return (
    <Dialog open={open} onOpenChange={(o) => (o ? onOpenChange(true) : handleClose())}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>Kiro SSO 登录（企业 / Azure 租户）</DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-2">
          <p className="text-sm text-muted-foreground">
            浏览器登录 Kiro。点击“开始登录”后会打开浏览器，完成登录并授权后本页会自动检测并落库账号。
          </p>

          {phase === 'idle' && (
            <div className="space-y-3">
              {/* 登录方式切换 */}
              <div className="flex gap-2">
                <Button
                  type="button"
                  size="sm"
                  variant={mode === 'social' ? 'default' : 'outline'}
                  className="flex-1"
                  onClick={() => setMode('social')}
                >
                  社交 / 企业 Azure
                </Button>
                <Button
                  type="button"
                  size="sm"
                  variant={mode === 'idc' ? 'default' : 'outline'}
                  className="flex-1"
                  onClick={() => setMode('idc')}
                >
                  IAM Identity Center
                </Button>
              </div>

              <SshTunnelHint />

              {mode === 'idc' && (
                <>
                  <label htmlFor="idcStartUrl" className="text-sm font-medium">
                    Start URL（必填）
                  </label>
                  <Input
                    id="idcStartUrl"
                    placeholder="https://d-xxxxxxxxxx.awsapps.com/start"
                    value={startUrl}
                    onChange={(e) => setStartUrl(e.target.value)}
                  />
                  <p className="text-xs text-muted-foreground">
                    AWS 访问门户地址（IAM Identity Center 的 Start URL）
                  </p>
                </>
              )}

              <label htmlFor="ssoRegion" className="text-sm font-medium">
                Region（可选）
              </label>
              <Input
                id="ssoRegion"
                placeholder="留空默认 us-east-1"
                value={region}
                onChange={(e) => setRegion(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                {mode === 'idc'
                  ? 'IAM Identity Center 所在 region（如 us-east-1、ap-southeast-2）'
                  : '登录 region，profileArn 会在首次调用时按实际 region 自动解析'}
              </p>
            </div>
          )}

          {phase === 'waiting' && (
            <div className="space-y-3">
              <div className="flex items-center gap-2 text-sm text-muted-foreground">
                <span className="inline-block h-3 w-3 animate-spin rounded-full border-2 border-current border-t-transparent" />
                等待在浏览器中完成登录…
              </div>
              <SshTunnelHint />
              {signInUrl && (
                <p className="text-xs break-all">
                  未自动打开？
                  <a
                    href={signInUrl}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="ml-1 text-primary underline"
                  >
                    点此手动打开登录页
                  </a>
                </p>
              )}

              {/* 手动回调兜底：无 SSH 隧道时，浏览器授权后跳转的 127.0.0.1:3128 页面打不开，
                  但地址栏 URL 有效。用户把整条 URL 粘贴到这里，服务端解析并完成登录。 */}
              {mode === 'idc' && (
                <div className="rounded-md border border-border bg-muted/40 p-3 space-y-2">
                  <p className="text-xs font-medium">没法用 SSH 隧道？手动粘贴回调地址</p>
                  <p className="text-xs text-muted-foreground">
                    在 AWS 授权后，浏览器会跳到 <code>127.0.0.1:3128/oauth/callback?code=...</code>
                    （可能显示无法访问）。直接复制浏览器<strong>地址栏的完整 URL</strong>粘贴到下面，点提交即可。
                  </p>
                  <Input
                    placeholder="http://127.0.0.1:3128/oauth/callback?code=...&state=..."
                    value={manualCallback}
                    onChange={(e) => setManualCallback(e.target.value)}
                    className="text-xs font-mono"
                  />
                  <Button
                    type="button"
                    size="sm"
                    className="w-full"
                    disabled={submittingCallback || !manualCallback.trim()}
                    onClick={handleSubmitCallback}
                  >
                    {submittingCallback ? '提交中…' : '提交回调地址'}
                  </Button>
                </div>
              )}
            </div>
          )}

          {phase === 'completed' && (
            <div className="text-sm text-green-600 dark:text-green-400">
              ✓ 登录成功，账号已添加。
            </div>
          )}

          {phase === 'error' && (
            <div className="text-sm text-destructive break-all">登录失败：{errorMsg}</div>
          )}
        </div>

        <DialogFooter>
          {phase === 'completed' ? (
            <Button type="button" onClick={handleClose}>
              完成
            </Button>
          ) : (
            <>
              <Button type="button" variant="outline" onClick={handleClose}>
                {isBusy ? '取消' : '关闭'}
              </Button>
              {(phase === 'idle' || phase === 'error') && (
                <Button type="button" onClick={handleStart}>
                  {phase === 'error' ? '重试' : '开始登录'}
                </Button>
              )}
            </>
          )}
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
