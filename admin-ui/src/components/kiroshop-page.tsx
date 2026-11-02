/**
 * KiroShop（kiro.yonghu.shop 商城）面板。
 *
 * 独立于「供应商」页：这家是商品制店铺（预定排队 / 增量拿号 / 补号 / 售后），
 * 后端走 /api/admin/kiroshop/*，见 src/kiroshop/。
 */
import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  AlertTriangle, BadgeCheck, Ban, CheckCheck, CircleDollarSign, Clock, Download, Gift,
  Loader2, MessageSquareWarning, PackageSearch, RefreshCw, ShoppingCart, Store, Truck, Wallet,
} from 'lucide-react'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Switch } from '@/components/ui/switch'
import { Tabs, TabsContent, TabsList, TabsTrigger } from '@/components/ui/tabs'
import * as ksApi from '@/api/kiroshop'
import { kiroshopErrorMessage } from '@/api/kiroshop'
import type { BuyOutcome, KiroProduct, LocalEvent, Quote, ShopOrder, TrackedOrder } from '@/types/kiroshop'

function formatTime(ts?: string | null): string {
  if (!ts) return '—'
  const d = new Date(ts)
  return Number.isNaN(d.getTime()) ? ts : d.toLocaleString('zh-CN', { hour12: false })
}

/** 商品可下单现货数（in_stock=false 一律按 0） */
function productAvailable(p: KiroProduct): number {
  if (p.stock?.in_stock === false) return 0
  return Math.max(0, p.stock?.available ?? 0)
}

function num(v: string): number | undefined {
  const t = v.trim()
  if (t === '') return undefined
  const n = Number(t)
  return Number.isFinite(n) ? n : undefined
}

function money(v?: number | null): string {
  if (v == null) return '—'
  return `¥${Number(v).toFixed(2)}`
}

const STATUS_META: Record<string, { text: string; cls: string }> = {
  pending: { text: '待支付', cls: 'border-amber-500/40 bg-amber-500/10 text-amber-600 dark:text-amber-400' },
  paid: { text: '已付款·待发货', cls: 'border-blue-500/40 bg-blue-500/10 text-blue-600 dark:text-blue-400' },
  out_of_stock: { text: '缺货排队', cls: 'border-violet-500/40 bg-violet-500/10 text-violet-600 dark:text-violet-400' },
  picked_up: { text: '已交付', cls: 'border-emerald-500/40 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400' },
  completed: { text: '已完成', cls: 'border-emerald-500/40 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400' },
  supplemented: { text: '已补号', cls: 'border-cyan-500/40 bg-cyan-500/10 text-cyan-600 dark:text-cyan-400' },
  timeout: { text: '超时关闭', cls: 'border-muted bg-muted text-muted-foreground' },
  failed: { text: '失败', cls: 'border-destructive/40 bg-destructive/10 text-destructive' },
  refunded: { text: '已退款', cls: 'border-destructive/40 bg-destructive/10 text-destructive' },
}

function StatusBadge({ status }: { status?: string | null }) {
  const s = status ?? ''
  const meta = STATUS_META[s] ?? { text: s || '—', cls: 'border-muted bg-muted text-muted-foreground' }
  return <Badge className={meta.cls}>{meta.text}</Badge>
}

// ============ 状态条 ============

function StatusBar() {
  const { data: st, isLoading, refetch, isFetching } = useQuery({
    queryKey: ['kiroshop', 'status'],
    queryFn: ksApi.getStatus,
    refetchInterval: 15_000,
  })

  if (isLoading) {
    return (
      <Card>
        <CardContent className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
          <Loader2 className="h-4 w-4 animate-spin" /> 加载 KiroShop 状态…
        </CardContent>
      </Card>
    )
  }
  if (!st?.configured) {
    return (
      <Card>
        <CardContent className="py-6 text-sm text-muted-foreground">
          KiroShop 未配置：在 config.json 加 <code className="mx-1 rounded bg-muted px-1">kiroshop</code> 段并填入
          <code className="mx-1 rounded bg-muted px-1">apiKey</code>（ks_live_ 开头），或从旧 vendors 条目迁移。
        </CardContent>
      </Card>
    )
  }

  const acc = st.account
  const items: { icon: React.ReactNode; label: string; value: string }[] = [
    { icon: <Wallet className="h-4 w-4" />, label: '余额', value: money(acc?.balance) },
    { icon: <CircleDollarSign className="h-4 w-4" />, label: '今日已扣', value: money(acc?.api_spent_today) },
    {
      icon: <Store className="h-4 w-4" />,
      label: '池内号 / 在途 / 目标',
      value: `${st.pool?.count ?? '—'} / ${st.pool?.inflight ?? '—'}${st.pool?.target ? ` / ${st.pool.target}` : ''}`,
    },
    { icon: <Truck className="h-4 w-4" />, label: '在途订单', value: String(st.trackedCount ?? '—') },
    {
      icon: <BadgeCheck className="h-4 w-4" />,
      label: '回调',
      value: acc?.webhook?.enabled ? '已启用' : '未启用',
    },
  ]
  if ((st.unackedAlerts ?? 0) > 0) {
    items.push({
      icon: <AlertTriangle className="h-4 w-4" />,
      label: '未确认告警',
      value: String(st.unackedAlerts),
    })
  }

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between py-3">
        <CardTitle className="flex items-center gap-2 text-sm font-medium">
          <Store className="h-4 w-4" /> KiroShop 商城
          <span className="text-xs font-normal text-muted-foreground">{st.baseUrl}</span>
        </CardTitle>
        <Button variant="ghost" size="sm" onClick={() => refetch()} disabled={isFetching}>
          <RefreshCw className={`h-4 w-4 ${isFetching ? 'animate-spin' : ''}`} />
        </Button>
      </CardHeader>
      <CardContent className="grid grid-cols-2 gap-3 md:grid-cols-3 lg:grid-cols-6">
        {items.map((it) => (
          <div key={it.label} className="flex items-center gap-2 rounded-lg border p-2.5">
            <span className="text-muted-foreground">{it.icon}</span>
            <div className="min-w-0">
              <div className="truncate text-[11px] text-muted-foreground">{it.label}</div>
              <div className="truncate text-sm font-medium">{it.value}</div>
            </div>
          </div>
        ))}
      </CardContent>
      {st.lastError && (
        <CardContent className="pt-0">
          <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-1.5 text-xs text-destructive">
            {st.lastError}
          </div>
        </CardContent>
      )}
    </Card>
  )
}

// ============ 商品与购买 ============

function BuyPanel({ product, onDone }: { product: KiroProduct; onDone: () => void }) {
  const queryClient = useQueryClient()
  const [quantity, setQuantity] = useState(1)
  const [optionKey, setOptionKey] = useState(
    product.options.find((o) => o.key === 'normal')?.key ?? product.options[0]?.key ?? '',
  )
  const [coupon, setCoupon] = useState('')
  const [allowBackorder, setAllowBackorder] = useState(false)
  const [quote, setQuote] = useState<Quote | null>(null)

  const quoteMut = useMutation({
    mutationFn: () =>
      ksApi.getQuote({
        productId: product.id,
        quantity,
        optionKey: optionKey || undefined,
        couponCode: coupon || undefined,
        allowBackorder,
      }),
    onSuccess: (q) => setQuote(q),
    onError: (e) => toast.error(kiroshopErrorMessage(e, '报价失败')),
  })

  const buyMut = useMutation({
    mutationFn: (maxAmount?: number) =>
      ksApi.buy({
        productId: product.id,
        quantity,
        optionKey: optionKey || undefined,
        couponCode: coupon || undefined,
        maxAmount,
        allowBackorder,
      }),
    onSuccess: (o: BuyOutcome) => {
      const imported = `入库 ${o.imported}、重复 ${o.duplicated}、失败 ${o.failed}`
      if (o.lastError) toast.warning(`订单 ${o.orderId.slice(0, 8)}：${o.lastError}（${imported}）`)
      else toast.success(`订单 ${o.orderId.slice(0, 8)} 下单完成：${o.statusText ?? o.status}，${imported}`)
      queryClient.invalidateQueries({ queryKey: ['kiroshop'] })
      onDone()
    },
    onError: (e) => toast.error(kiroshopErrorMessage(e, '下单失败')),
  })

  const stock = product.stock
  return (
    <div className="mt-2 space-y-2 rounded-md border bg-muted/30 p-3">
      <div className="flex flex-wrap items-center gap-2 text-sm">
        <span className="text-muted-foreground">数量</span>
        <Input
          type="number" min={1} max={999} value={quantity} className="h-8 w-20"
          onChange={(e) => setQuantity(Math.max(1, Number(e.target.value) || 1))}
        />
        {product.options.length > 0 && (
          <Select value={optionKey} onValueChange={setOptionKey}>
            <SelectTrigger className="h-8 w-28"><SelectValue placeholder="档位" /></SelectTrigger>
            <SelectContent>
              {product.options.map((o) => (
                <SelectItem key={o.key} value={o.key}>
                  {o.label ?? o.key}{o.price != null ? ` ¥${o.price}` : ''}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        )}
        <Input
          placeholder="优惠码（可选）" value={coupon} className="h-8 w-36"
          onChange={(e) => setCoupon(e.target.value)}
        />
        <label className="flex items-center gap-1.5 text-xs text-muted-foreground">
          <Checkbox checked={allowBackorder} onCheckedChange={(v) => setAllowBackorder(v === true)} />
          缺货转预定排队
        </label>
        <Button variant="outline" size="sm" disabled={quoteMut.isPending}
                onClick={() => { setQuote(null); quoteMut.mutate() }}>
          {quoteMut.isPending && <Loader2 className="mr-1 h-3 w-3 animate-spin" />} 报价
        </Button>
      </div>

      {quote && (
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 rounded-md border bg-background px-3 py-2 text-xs">
          <span>应付 <b>{money(quote.amount)}</b></span>
          {quote.discount_amount ? <span className="text-emerald-600">优惠 {money(quote.discount_amount)}</span> : null}
          <span>现货 {quote.in_stock_now ?? '—'}</span>
          {quote.backorder && <span className="text-violet-500">可预定（{quote.eta ?? 'eta 未知'}）</span>}
          <span>质保 {quote.warranty_hours ?? '—'}h</span>
          <span>余额 {money(quote.balance)}{quote.balance_enough === false && <b className="text-destructive">（不足）</b>}</span>
        </div>
      )}

      <div className="flex items-center gap-2">
        <Button
          size="sm" disabled={buyMut.isPending}
          onClick={() => buyMut.mutate(quote?.amount ?? undefined)}
        >
          {buyMut.isPending ? <Loader2 className="mr-1 h-3 w-3 animate-spin" /> : <ShoppingCart className="mr-1 h-3 w-3" />}
          下单并扣款{quote?.amount != null ? ` ${money(quote.amount)}` : ''}
        </Button>
        <span className="text-[11px] text-muted-foreground">
          {stock.in_stock === false
            ? '当前无现货' + (stock.backorder ? '，可预定排队' : '')
            : stock.backorder
              ? '现货不足时可转预定'
              : ''}
        </span>
      </div>
    </div>
  )
}

function ProductsSection() {
  const { data: products, isLoading } = useQuery({
    queryKey: ['kiroshop', 'products'],
    queryFn: ksApi.getProducts,
    refetchInterval: 60_000,
  })
  const [expanded, setExpanded] = useState<number | null>(null)

  if (isLoading) {
    return <div className="flex items-center gap-2 py-8 text-sm text-muted-foreground"><Loader2 className="h-4 w-4 animate-spin" /> 加载商品…</div>
  }
  return (
    <div className="space-y-2">
      {(products ?? []).map((p) => (
        <Card key={p.id}>
          <CardContent className="py-3">
            <div
              className="flex cursor-pointer flex-wrap items-center justify-between gap-2"
              onClick={() => setExpanded(expanded === p.id ? null : p.id)}
            >
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <span className="font-medium">{p.display_name || `商品 #${p.id}`}</span>
                  <Badge variant="secondary">#{p.id}</Badge>
                  {p.warranty_mode && (
                    <span className="text-[11px] text-muted-foreground">质保 {p.warranty_mode}{p.warranty_hours ? ` ${p.warranty_hours}h` : ''}</span>
                  )}
                </div>
                <div className="mt-0.5 text-xs text-muted-foreground">
                  {p.options.map((o) => `${o.label ?? o.key}${o.price != null ? ` ¥${o.price}` : ''}`).join(' / ') || `¥${p.price ?? '—'}`}
                </div>
              </div>
              <div className="flex items-center gap-3 text-sm">
                <span className={productAvailable(p) > 0 ? '' : 'text-muted-foreground'}>
                  现货 {productAvailable(p)}
                </span>
                {p.stock.backorder && <Badge className="border-violet-500/40 bg-violet-500/10 text-violet-600 dark:text-violet-400">可预定</Badge>}
                <Button variant="outline" size="sm">
                  {expanded === p.id ? '收起' : '购买'}
                </Button>
              </div>
            </div>
            {expanded === p.id && <BuyPanel product={p} onDone={() => setExpanded(null)} />}
          </CardContent>
        </Card>
      ))}
      {(products ?? []).length === 0 && (
        <div className="py-8 text-center text-sm text-muted-foreground">没有上架商品</div>
      )}
    </div>
  )
}

// ============ 订单 ============

function OrderDetailDialog({ orderId, onClose }: { orderId: string; onClose: () => void }) {
  const queryClient = useQueryClient()
  const { data, isLoading } = useQuery({
    queryKey: ['kiroshop', 'order', orderId],
    queryFn: () => ksApi.getOrderDetail(orderId),
    enabled: !!orderId,
  })
  const [afterSaleKind, setAfterSaleKind] = useState('banned')
  const [afterSaleReason, setAfterSaleReason] = useState('')

  const refresh = () => queryClient.invalidateQueries({ queryKey: ['kiroshop'] })

  const pickupMut = useMutation({
    mutationFn: () => ksApi.pickupOrder(orderId),
    onSuccess: (o) => {
      toast.success(`取号完成：入库 ${o.imported}、重复 ${o.duplicated}、失败 ${o.failed}`)
      refresh()
    },
    onError: (e) => toast.error(kiroshopErrorMessage(e, '取号失败')),
  })
  const importMut = useMutation({
    mutationFn: () => ksApi.importDelivery(orderId),
    onSuccess: (o) => {
      if (o.imported > 0) toast.success(`导入 ${o.imported} 条凭据`)
      else toast.info('没有新到货的内容')
      refresh()
    },
    onError: (e) => toast.error(kiroshopErrorMessage(e, '导入失败')),
  })
  const supplementMut = useMutation({
    mutationFn: (sid: number) => ksApi.pickupSupplement(orderId, sid),
    onSuccess: (o) => {
      toast.success(`补号已领取入库：${o.imported} 条`)
      refresh()
    },
    onError: (e) => toast.error(kiroshopErrorMessage(e, '领取失败')),
  })
  const afterSaleMut = useMutation({
    mutationFn: () => ksApi.createAfterSale(orderId, afterSaleKind, afterSaleReason || undefined),
    onSuccess: () => {
      toast.success('售后申请已提交')
      setAfterSaleReason('')
      refresh()
    },
    onError: (e) => toast.error(kiroshopErrorMessage(e, '提交失败')),
  })

  const order = data?.order
  return (
    <Dialog open={!!orderId} onOpenChange={(v) => !v && onClose()}>
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2 text-base">
            <PackageSearch className="h-4 w-4" /> 订单 {orderId.slice(0, 13)}…
          </DialogTitle>
          <DialogDescription>交付、补号与售后</DialogDescription>
        </DialogHeader>
        {isLoading || !order ? (
          <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground"><Loader2 className="h-4 w-4 animate-spin" /> 加载…</div>
        ) : (
          <div className="space-y-4 text-sm">
            <div className="grid grid-cols-2 gap-x-4 gap-y-1.5">
              <div>商品：{order.product_name}</div>
              <div>数量：{order.quantity}（已发 {order.delivered}）</div>
              <div>金额：{money(order.amount)}</div>
              <div className="flex items-center gap-1.5">状态：<StatusBadge status={order.status} />{order.status_text}</div>
              <div>档位：{order.selected_option_key ?? '—'}</div>
              <div>创建：{formatTime(order.created_at)}</div>
              {order.eta && <div className="col-span-2 text-violet-500">预计：{order.eta}</div>}
              {data?.tracked?.lastError && (
                <div className="col-span-2 text-destructive">跟踪问题：{data.tracked.lastError}</div>
              )}
            </div>

            <div className="flex flex-wrap gap-2">
              <Button size="sm" variant="outline" onClick={() => pickupMut.mutate()} disabled={pickupMut.isPending}>
                {pickupMut.isPending ? <Loader2 className="mr-1 h-3 w-3 animate-spin" /> : <Truck className="mr-1 h-3 w-3" />}
                取号并入库
              </Button>
              <Button size="sm" variant="outline" onClick={() => importMut.mutate()} disabled={importMut.isPending}>
                <Download className="mr-1 h-3 w-3" /> 增量导入
              </Button>
            </div>

            <div>
              <div className="mb-1.5 flex items-center gap-1.5 font-medium"><Gift className="h-3.5 w-3.5" /> 补号（领取即结束原单质保）</div>
              {data.supplements.length === 0 ? (
                <div className="text-xs text-muted-foreground">无补号记录</div>
              ) : (
                <div className="space-y-1">
                  {data.supplements.map((s, i) => {
                    const sid = Number(s.id ?? s.supplement_id ?? -1)
                    return (
                      <div key={i} className="flex items-center justify-between rounded-md border px-3 py-1.5 text-xs">
                        <span className="truncate text-muted-foreground">{JSON.stringify(s).slice(0, 120)}</span>
                        <Button size="sm" variant="outline" disabled={sid < 0 || supplementMut.isPending}
                                onClick={() => supplementMut.mutate(sid)}>
                          领取
                        </Button>
                      </div>
                    )
                  })}
                </div>
              )}
            </div>

            <div>
              <div className="mb-1.5 flex items-center gap-1.5 font-medium"><MessageSquareWarning className="h-3.5 w-3.5" /> 售后</div>
              {data.afterSale.length > 0 && (
                <pre className="mb-2 max-h-32 overflow-auto rounded-md bg-muted p-2 text-[11px] leading-relaxed">
                  {JSON.stringify(data.afterSale, null, 1)}
                </pre>
              )}
              <div className="flex flex-wrap items-center gap-2">
                <Select value={afterSaleKind} onValueChange={setAfterSaleKind}>
                  <SelectTrigger className="h-8 w-28"><SelectValue /></SelectTrigger>
                  <SelectContent>
                    <SelectItem value="banned">封号</SelectItem>
                    <SelectItem value="login">无法登录</SelectItem>
                    <SelectItem value="quota">额度问题</SelectItem>
                    <SelectItem value="missing">少发</SelectItem>
                    <SelectItem value="other">其他</SelectItem>
                  </SelectContent>
                </Select>
                <Input
                  placeholder="补充说明（可选）" value={afterSaleReason} className="h-8 flex-1"
                  onChange={(e) => setAfterSaleReason(e.target.value)}
                />
                <Button size="sm" variant="outline" disabled={afterSaleMut.isPending} onClick={() => afterSaleMut.mutate()}>
                  提交售后
                </Button>
              </div>
            </div>
          </div>
        )}
      </DialogContent>
    </Dialog>
  )
}

function OrdersSection() {
  const { data, isLoading } = useQuery({
    queryKey: ['kiroshop', 'orders'],
    queryFn: ksApi.getOrders,
    refetchInterval: 30_000,
  })
  const queryClient = useQueryClient()
  const [detailId, setDetailId] = useState('')

  const payMut = useMutation({
    mutationFn: (id: string) => ksApi.payOrder(id),
    onSuccess: () => { toast.success('已付款'); queryClient.invalidateQueries({ queryKey: ['kiroshop'] }) },
    onError: (e) => toast.error(kiroshopErrorMessage(e, '付款失败')),
  })
  const cancelMut = useMutation({
    mutationFn: (id: string) => ksApi.cancelOrder(id),
    onSuccess: () => { toast.success('已取消'); queryClient.invalidateQueries({ queryKey: ['kiroshop'] }) },
    onError: (e) => toast.error(kiroshopErrorMessage(e, '取消失败')),
  })

  if (isLoading) {
    return <div className="flex items-center gap-2 py-8 text-sm text-muted-foreground"><Loader2 className="h-4 w-4 animate-spin" /> 加载订单…</div>
  }

  const tracked = data?.tracked ?? []
  const trackedIds = new Set(tracked.map((t) => t.orderId))
  const untracked: ShopOrder[] = (data?.shop ?? []).filter(
    (o) => !trackedIds.has(o.id) && o.product_id !== 0,
  )

  const renderRow = (t: TrackedOrder) => (
    <tr key={t.orderId} className="border-b last:border-0">
      <td className="py-2 pr-2 font-mono text-xs">{t.orderId.slice(0, 8)}</td>
      <td className="py-2 pr-2">{t.productName}</td>
      <td className="py-2 pr-2 text-center">
        {t.delivered}/{t.quantity}
        <span className="ml-1 text-[11px] text-muted-foreground">入{t.imported}</span>
      </td>
      <td className="py-2 pr-2 text-right">{money(t.amount)}</td>
      <td className="py-2 pr-2"><StatusBadge status={t.status} /></td>
      <td className="py-2 pr-2 text-xs text-muted-foreground">
        {t.eta ?? (t.status === 'paid' || t.status === 'out_of_stock' ? <Clock className="inline h-3 w-3" /> : '—')}
      </td>
      <td className="py-2 pl-2 text-right">
        <div className="flex justify-end gap-1">
          {t.status === 'pending' && (
            <>
              <Button size="sm" variant="outline" onClick={() => payMut.mutate(t.orderId)}>付款</Button>
              <Button size="sm" variant="ghost" onClick={() => cancelMut.mutate(t.orderId)}><Ban className="h-3 w-3" /></Button>
            </>
          )}
          <Button size="sm" variant="outline" onClick={() => setDetailId(t.orderId)}>详情</Button>
        </div>
      </td>
    </tr>
  )

  return (
    <div className="space-y-4">
      <div className="overflow-x-auto rounded-lg border">
        <table className="w-full text-sm">
          <thead>
            <tr className="border-b bg-muted/50 text-left text-xs text-muted-foreground">
              <th className="px-3 py-2 font-medium">订单</th>
              <th className="px-3 py-2 font-medium">商品</th>
              <th className="px-3 py-2 font-medium text-center">交付/数量</th>
              <th className="px-3 py-2 font-medium text-right">金额</th>
              <th className="px-3 py-2 font-medium">状态</th>
              <th className="px-3 py-2 font-medium">预计</th>
              <th className="px-3 py-2 font-medium text-right">操作</th>
            </tr>
          </thead>
          <tbody>
            {tracked.map(renderRow)}
            {untracked.map((o) => (
              <tr key={o.id} className="border-b last:border-0 opacity-70">
                <td className="py-2 pr-2 font-mono text-xs">{o.id.slice(0, 8)}</td>
                <td className="py-2 pr-2">{o.product_name}</td>
                <td className="py-2 pr-2 text-center">{o.delivered}/{o.quantity}</td>
                <td className="py-2 pr-2 text-right">{money(o.amount)}</td>
                <td className="py-2 pr-2"><StatusBadge status={o.status} /></td>
                <td className="py-2 pr-2 text-xs text-muted-foreground">未纳管</td>
                <td className="py-2 pl-2 text-right">
                  <Button size="sm" variant="outline" onClick={() => setDetailId(o.id)}>详情</Button>
                </td>
              </tr>
            ))}
            {tracked.length === 0 && untracked.length === 0 && (
              <tr><td colSpan={7} className="py-8 text-center text-muted-foreground">暂无订单</td></tr>
            )}
          </tbody>
        </table>
      </div>
      <OrderDetailDialog orderId={detailId} onClose={() => setDetailId('')} />
    </div>
  )
}

// ============ 事件 ============

function EventsSection() {
  const queryClient = useQueryClient()
  const { data } = useQuery({
    queryKey: ['kiroshop', 'events'],
    queryFn: () => ksApi.getLocalEvents(100),
    refetchInterval: 20_000,
  })
  const ackMut = useMutation({
    mutationFn: (id?: number) => ksApi.ackLocalEvents(id),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['kiroshop', 'events'] }),
  })
  const evs: LocalEvent[] = data?.data ?? []
  return (
    <div className="space-y-2">
      <div className="flex items-center justify-between">
        <div className="text-xs text-muted-foreground">
          未确认 {data?.unacked ?? 0} 条（余额低 / 补号可领 / 售后回复 / 自动补货 …）
        </div>
        <Button size="sm" variant="outline" onClick={() => ackMut.mutate(undefined)}>
          <CheckCheck className="mr-1 h-3 w-3" /> 全部确认
        </Button>
      </div>
      {evs.length === 0 ? (
        <div className="py-8 text-center text-sm text-muted-foreground">暂无事件</div>
      ) : (
        evs.map((ev) => (
          <div
            key={ev.id}
            className={`flex items-start justify-between gap-2 rounded-md border px-3 py-2 text-sm ${
              ev.severity === 'error'
                ? 'border-destructive/40 bg-destructive/5'
                : ev.severity === 'warn'
                  ? 'border-amber-500/40 bg-amber-500/5'
                  : ''
            } ${ev.acked ? 'opacity-60' : ''}`}
          >
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <Badge variant="secondary">{ev.kind}</Badge>
                <span className="text-xs text-muted-foreground">{formatTime(ev.createdAt)}</span>
              </div>
              <div className="mt-1 break-all text-sm">{ev.message}</div>
            </div>
            {!ev.acked && (
              <Button size="sm" variant="ghost" onClick={() => ackMut.mutate(ev.id)}>
                <CheckCheck className="h-3 w-3" />
              </Button>
            )}
          </div>
        ))
      )}
    </div>
  )
}

// ============ 设置 ============

function SettingsSection() {
  const queryClient = useQueryClient()
  const { data: cfg } = useQuery({ queryKey: ['kiroshop', 'settings'], queryFn: ksApi.getSettings })
  const saveMut = useMutation({
    mutationFn: (patch: Record<string, unknown>) => ksApi.updateSettings(patch),
    onSuccess: () => {
      toast.success('已保存')
      queryClient.invalidateQueries({ queryKey: ['kiroshop'] })
    },
    onError: (e) => toast.error(kiroshopErrorMessage(e, '保存失败')),
  })

  const [whUrl, setWhUrl] = useState('')
  const { data: webhook } = useQuery({ queryKey: ['kiroshop', 'webhook'], queryFn: ksApi.getWebhook })
  const whMut = useMutation<unknown, Error, 'put' | 'test' | 'rotate' | 'delete'>({
    mutationFn: async (fn) =>
      fn === 'put' ? ksApi.putWebhook(whUrl.trim())
      : fn === 'test' ? ksApi.testWebhook()
      : fn === 'rotate' ? ksApi.rotateWebhookSecret()
      : ksApi.deleteWebhook(),
    onSuccess: (v, fn) => {
      if (fn === 'put') toast.success('回调已设置（签名密钥已自动保存）')
      else if (fn === 'rotate') toast.success('密钥已轮换并保存')
      else if (fn === 'delete') toast.success('回调已删除')
      else toast.info(JSON.stringify(v))
      queryClient.invalidateQueries({ queryKey: ['kiroshop', 'webhook'] })
    },
    onError: (e) => toast.error(kiroshopErrorMessage(e)),
  })

  if (!cfg) {
    return <div className="flex items-center gap-2 py-8 text-sm text-muted-foreground"><Loader2 className="h-4 w-4 animate-spin" /> 加载设置…</div>
  }

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader className="py-3"><CardTitle className="text-sm">基础</CardTitle></CardHeader>
        <CardContent className="space-y-3 text-sm">
          <div className="flex flex-wrap items-center gap-4">
            <label className="flex items-center gap-2">
              <Switch checked={cfg.enabled} onCheckedChange={(v) => saveMut.mutate({ enabled: v })} />
              启用对接
            </label>
            <span className="text-xs text-muted-foreground">apiKey：{cfg.apiKeyMasked || '未配置'}</span>
            <span className="text-xs text-muted-foreground">回调路径：{cfg.webhookPath}</span>
          </div>
          <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
            <SettingNumberField label="事件轮询（秒）" value={cfg.eventsPollIntervalSecs}
                                 onCommit={(v) => saveMut.mutate({ eventsPollIntervalSecs: v })} />
            <SettingNumberField label="发货轮询（秒）" value={cfg.deliveryPollIntervalSecs}
                                 onCommit={(v) => saveMut.mutate({ deliveryPollIntervalSecs: v })} />
            <SettingNumberField label="余额告警线（元）" value={cfg.balanceAlertThreshold ?? 0} allowEmpty
                                 onCommit={(v) => saveMut.mutate({ balanceAlertThreshold: v ?? 0 })} />
            <SettingNumberField label="卖家每日扣款上限" value={0} allowEmpty placeholder="0 = 不限"
                                 onCommit={(v) => v != null && ksApi.setDailyLimit(v).then(
                                   () => toast.success('每日限额已更新'),
                                   (e) => toast.error(kiroshopErrorMessage(e)),
                                 )} />
          </div>
          <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
            <SettingTextField label="入库分组（逗号分隔）" value={cfg.groups.join(',')}
                              onCommit={(v) => saveMut.mutate({ groups: v.split(',').map((s) => s.trim()).filter(Boolean) })} />
            <SettingNumberField label="RPM 上限" value={cfg.rpmLimit}
                                 onCommit={(v) => saveMut.mutate({ rpmLimit: v ?? 5 })} />
            <SettingNumberField label="调度优先级（小=优先）" value={cfg.priority}
                                 onCommit={(v) => saveMut.mutate({ priority: v ?? 15 })} />
            <SettingTextField label="apiRegion（跨区号才需要）" value={cfg.apiRegion}
                              onCommit={(v) => saveMut.mutate({ apiRegion: v })} />
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="py-3"><CardTitle className="text-sm">自动补货</CardTitle></CardHeader>
        <CardContent className="space-y-3 text-sm">
          <div className="flex flex-wrap items-center gap-4">
            <label className="flex items-center gap-2">
              <Switch checked={cfg.autoBuy.enabled}
                      onCheckedChange={(v) => saveMut.mutate({ autoBuy: { ...cfg.autoBuy, enabled: v } })} />
              启用（池内号 + 在途 &lt; 目标时自动下单）
            </label>
            <label className="flex items-center gap-2 text-xs text-muted-foreground">
              <Checkbox checked={cfg.autoBuy.allowBackorder}
                        onCheckedChange={(v) => saveMut.mutate({ autoBuy: { ...cfg.autoBuy, allowBackorder: v === true } })} />
              允许转预定排队
            </label>
          </div>
          <div className="grid grid-cols-2 gap-3 md:grid-cols-4">
            <SettingNumberField label="池目标数" value={cfg.autoBuy.poolTarget}
                                 onCommit={(v) => saveMut.mutate({ autoBuy: { ...cfg.autoBuy, poolTarget: v ?? 0 } })} />
            <SettingNumberField label="单笔上限（个）" value={cfg.autoBuy.maxPerOrder}
                                 onCommit={(v) => saveMut.mutate({ autoBuy: { ...cfg.autoBuy, maxPerOrder: v ?? 5 } })} />
            <SettingNumberField label="单价上限（元）" value={cfg.autoBuy.maxUnitPrice ?? 0} allowEmpty placeholder="不限"
                                 onCommit={(v) => saveMut.mutate({ autoBuy: { ...cfg.autoBuy, maxUnitPrice: v ?? 0 } })} />
            <SettingNumberField label="每日预算（元）" value={cfg.autoBuy.dailyBudget ?? 0} allowEmpty placeholder="不限"
                                 onCommit={(v) => saveMut.mutate({ autoBuy: { ...cfg.autoBuy, dailyBudget: v ?? 0 } })} />
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="py-3"><CardTitle className="text-sm">到货回调（webhook）</CardTitle></CardHeader>
        <CardContent className="space-y-3 text-sm">
          <div className="text-xs text-muted-foreground">
            {webhook?.enabled ? `已启用 → ${String(webhook.url ?? '')}` : '未启用'}；
            本机路径 <code className="rounded bg-muted px-1">{String(webhook?.localPath ?? cfg.webhookPath)}</code>
            （拼上公网域名填到下面）；{cfg.webhookSecretSet ? '签名密钥已保存' : '签名密钥未配置（首次设置回调时自动捕获）'}
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <Input
              placeholder="https://你的域名/webhook/kiroshop/<token>"
              value={whUrl} className="h-8 flex-1" onChange={(e) => setWhUrl(e.target.value)}
            />
            <Button size="sm" disabled={!whUrl.trim().startsWith('https://') || whMut.isPending}
                    onClick={() => whMut.mutate('put')}>
              保存回调
            </Button>
            <Button size="sm" variant="outline" onClick={() => whMut.mutate('test')} disabled={whMut.isPending}>发测试 ping</Button>
            <Button size="sm" variant="outline" onClick={() => whMut.mutate('rotate')} disabled={whMut.isPending}>轮换密钥</Button>
            <Button size="sm" variant="ghost" onClick={() => whMut.mutate('delete')} disabled={whMut.isPending}>删除</Button>
          </div>
        </CardContent>
      </Card>
    </div>
  )
}

/** 数字设置项：失焦/回车提交 */
function SettingNumberField({ label, value, onCommit, allowEmpty, placeholder }:
  { label: string; value: number; onCommit: (v: number | undefined) => void; allowEmpty?: boolean; placeholder?: string }) {
  const [text, setText] = useState(String(value ?? ''))
  return (
    <div>
      <div className="mb-1 text-[11px] text-muted-foreground">{label}</div>
      <Input
        type="number" className="h-8" value={text} placeholder={placeholder}
        onChange={(e) => setText(e.target.value)}
        onBlur={() => {
          const n = num(text)
          if (allowEmpty && text.trim() === '') onCommit(undefined)
          else if (n != null && Number.isFinite(n) && n !== value) onCommit(n)
        }}
        onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
      />
    </div>
  )
}

function SettingTextField({ label, value, onCommit }: { label: string; value: string; onCommit: (v: string) => void }) {
  const [text, setText] = useState(value)
  return (
    <div>
      <div className="mb-1 text-[11px] text-muted-foreground">{label}</div>
      <Input
        className="h-8" value={text}
        onChange={(e) => setText(e.target.value)}
        onBlur={() => text !== value && onCommit(text)}
        onKeyDown={(e) => e.key === 'Enter' && (e.target as HTMLInputElement).blur()}
      />
    </div>
  )
}

// ============ 页面 ============

export function KiroshopPage() {
  return (
    <div className="space-y-4">
      <StatusBar />
      <Tabs defaultValue="products">
        <TabsList>
          <TabsTrigger value="products">商品</TabsTrigger>
          <TabsTrigger value="orders">订单</TabsTrigger>
          <TabsTrigger value="events">事件</TabsTrigger>
          <TabsTrigger value="settings">设置</TabsTrigger>
        </TabsList>
        <TabsContent value="products" className="mt-3"><ProductsSection /></TabsContent>
        <TabsContent value="orders" className="mt-3"><OrdersSection /></TabsContent>
        <TabsContent value="events" className="mt-3"><EventsSection /></TabsContent>
        <TabsContent value="settings" className="mt-3"><SettingsSection /></TabsContent>
      </Tabs>
    </div>
  )
}
