import { useEffect, useState } from 'react'
import { Loader2, Play } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Textarea } from '@/components/ui/textarea'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { useCredentialModels } from '@/hooks/use-credentials'
import { testCredentialModel } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import type { CredentialModelTestResponse } from '@/types/api'

interface CredentialModelTestDialogProps {
  credentialId: number | null
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function CredentialModelTestDialog({
  credentialId,
  open,
  onOpenChange,
}: CredentialModelTestDialogProps) {
  const { data, isLoading, error: modelsError } = useCredentialModels(
    open ? credentialId : null,
  )
  const [model, setModel] = useState('')
  const [message, setMessage] = useState('Reply with OK only.')
  const [isTesting, setIsTesting] = useState(false)
  const [result, setResult] = useState<CredentialModelTestResponse | null>(null)
  const [testError, setTestError] = useState<string | null>(null)

  useEffect(() => {
    if (!open) return
    setResult(null)
    setTestError(null)
  }, [open, credentialId])

  useEffect(() => {
    const models = data?.models ?? []
    if (models.length > 0 && !models.some((item) => item.modelId === model)) {
      setModel(models[0].modelId)
    }
  }, [data, model])

  const runTest = async () => {
    if (credentialId === null || !model || isTesting) return
    setIsTesting(true)
    setResult(null)
    setTestError(null)
    try {
      setResult(await testCredentialModel(credentialId, { model, message }))
    } catch (error) {
      setTestError(extractErrorMessage(error))
    } finally {
      setIsTesting(false)
    }
  }

  const models = data?.models ?? []

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>凭据 #{credentialId} 模型测试</DialogTitle>
          <DialogDescription>
            使用该凭据实际调用所选模型，不会切换到其他凭据。
          </DialogDescription>
        </DialogHeader>

        <div className="space-y-4">
          <div className="space-y-1.5">
            <label className="text-sm font-medium">模型</label>
            <Select value={model} onValueChange={setModel} disabled={isLoading || models.length === 0}>
              <SelectTrigger className="h-10 rounded-xl px-3.5">
                <SelectValue placeholder={isLoading ? '正在获取模型...' : '选择模型'} />
              </SelectTrigger>
              <SelectContent>
                {models.map((item) => (
                  <SelectItem key={item.modelId} value={item.modelId}>
                    {item.modelName || item.modelId}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
            {modelsError && (
              <p className="text-xs text-destructive">{extractErrorMessage(modelsError)}</p>
            )}
            {!isLoading && !modelsError && data && models.length === 0 && (
              <p className="text-xs text-muted-foreground">该凭据当前没有可用模型</p>
            )}
          </div>

          <div className="space-y-1.5">
            <label className="text-sm font-medium" htmlFor="credential-test-message">测试消息</label>
            <Textarea
              id="credential-test-message"
              value={message}
              onChange={(event) => setMessage(event.target.value)}
              maxLength={2000}
              className="min-h-24"
              disabled={isTesting}
            />
          </div>

          {testError && (
            <div className="rounded-md border border-destructive/40 bg-destructive/5 px-3 py-2 text-sm text-destructive">
              {testError}
            </div>
          )}

          {result && (
            <div className="space-y-2 border-t pt-4">
              <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
                <span>{result.model}</span>
                <span>{result.latencyMs} ms</span>
                <span>{result.credits.toFixed(6)} credits</span>
              </div>
              <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-words rounded-md bg-muted px-3 py-2 text-sm">
                {result.reply || '(上游成功，但没有返回文本)'}
              </pre>
            </div>
          )}
        </div>

        <DialogFooter>
          <Button
            onClick={runTest}
            disabled={isTesting || !model || !message.trim()}
          >
            {isTesting ? <Loader2 className="animate-spin" /> : <Play />}
            {isTesting ? '调用中' : '实际调用'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
