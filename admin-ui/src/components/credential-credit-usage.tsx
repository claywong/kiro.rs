/** 本站累计消耗；@author wangzhong */
export function CredentialCreditUsage({ credits }: { credits?: number }) {
  const value = credits != null && Number.isFinite(credits)
    ? credits.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 3 })
    : "—";

  return (
    <div
      className="flex min-w-0 flex-col items-center justify-center px-1 text-center"
      title="本站启用累计统计后，上游实际计量的 credit 消耗；包含已产生费用的失败请求，不随成功次数重置"
    >
      <span className="text-[10px] font-semibold text-muted-foreground/80">
        已用 Credit
      </span>
      <span className="mt-0.5 max-w-full break-all font-mono text-xs font-semibold tabular-nums">
        {value}
      </span>
    </div>
  );
}
