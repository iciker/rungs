<template>
  <div>
    <n-h2>仪表盘</n-h2>

    <n-grid :cols="3" :x-gap="16" :y-gap="16" style="margin-bottom: 24px">
      <n-gi>
        <n-statistic label="累计利润 (USDT)" :value="stats?.total_profit ?? '—'" />
      </n-gi>
      <n-gi>
        <n-statistic label="稳定币余额 (USD)" :value="stableBalance ?? '—'">
          <template v-if="excludedAssets.length" #suffix>
            <n-text depth="3" style="font-size: 12px">
              （未计入 {{ excludedAssets.join('、') }}）
            </n-text>
          </template>
        </n-statistic>
      </n-gi>
      <n-gi>
        <n-statistic label="成交次数" :value="stats?.trade_count ?? '—'" />
      </n-gi>
    </n-grid>

    <!-- Binance 现货余额 -->
    <n-h3>Binance 现货余额（可交易）</n-h3>
    <n-spin :show="balanceLoading">
      <n-space v-if="balances.length > 0" style="margin-bottom: 24px">
        <n-tag v-for="b in balances" :key="b.asset" size="large" :bordered="false" type="info">
          {{ b.asset }}: <strong>{{ b.free }}</strong>
          <span v-if="+b.locked > 0" style="opacity:0.6"> (锁定: {{ b.locked }})</span>
        </n-tag>
        <n-tag v-if="usdcusdPrice" size="large" :bordered="false" type="success">
          USDC/USD: <strong>{{ usdcusdPrice }}</strong>
        </n-tag>
      </n-space>
      <n-empty v-else-if="!balanceLoading" description="无法获取余额或账户为空" style="margin-bottom: 24px" />
    </n-spin>

    <n-h3>各 Symbol 网格状态</n-h3>
    <n-spin :show="loading">
      <div v-for="group in groups" :key="group.name" style="margin-bottom: 20px">
        <n-text strong style="font-size: 15px; display: block; margin-bottom: 8px">{{ group.name }}</n-text>
        <n-data-table :columns="columns" :data="group.grid" :pagination="false" size="small" />
      </div>
      <n-empty v-if="!loading && groups.length === 0" description="暂无网格，请先在「网格管理」中创建" />
    </n-spin>
  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted } from 'vue'
import { NH2, NH3, NGrid, NGi, NStatistic, NDataTable, NSpin, NText, NEmpty, NTag, NSpace } from 'naive-ui'
import type { DataTableColumns } from 'naive-ui'
import { gridApi, statsApi, balanceApi, priceApi } from '@/api'
import type { UserGrid, GridGroup, ProfitStats, SpotBalance } from '@/api'

const groups = ref<GridGroup[]>([])
const stats = ref<ProfitStats | null>(null)
const loading = ref(false)
const balances = ref<SpotBalance[]>([])
const balanceLoading = ref(false)
const usdcusdPrice = ref<string | null>(null)

/** 已知汇率为 1 附近的稳定币；其余资产不参与这个统计口径 */
const STABLECOINS = ['USDT', 'USD', 'BUSD', 'USDC'] as const

/**
 * 稳定币余额合计（折合 USD）。
 *
 * 只统计稳定币，因为前端此刻手上只有 USDCUSD 一个汇率。
 * 不要把非稳定币按 1.0 计入：持有 0.5 BTC 会显示成 0.5，与真实价值差五个数量级。
 * 要做全量估值，需为每个非稳定币各取一次 ticker。
 */
const stableBalance = computed(() => {
  if (balances.value.length === 0) return null
  const usdcRate = usdcusdPrice.value ? parseFloat(usdcusdPrice.value) : 1
  const sum = balances.value
    .filter((b) => STABLECOINS.includes(b.asset as (typeof STABLECOINS)[number]))
    .reduce((acc, b) => {
      const amount = parseFloat(b.free) + parseFloat(b.locked)
      return acc + amount * (b.asset === 'USDC' ? usdcRate : 1)
    }, 0)
  return sum.toFixed(4)
})

/** 未被计入上面统计的资产（提示用户该数字不是全部身家） */
const excludedAssets = computed(() =>
  balances.value
    .filter((b) => !STABLECOINS.includes(b.asset as (typeof STABLECOINS)[number]))
    .map((b) => b.asset),
)

const columns: DataTableColumns<UserGrid> = [
  { title: 'ID', key: 'id', width: 60 },
  { title: '方向', key: 'side', width: 70 },
  { title: '买入价', key: 'buy_price' },
  { title: '卖出价', key: 'sell_price' },
  { title: '金额', key: 'amount' },
  { title: '状态', key: 'status', width: 90 },
  { title: '订单号', key: 'order_id', ellipsis: true },
]

onMounted(async () => {
  loading.value = true
  balanceLoading.value = true
  const [gridsResult, statsResult, balancesResult, priceResult] = await Promise.allSettled([
    gridApi.list(),
    statsApi.profitStats(),
    balanceApi.spot(),
    priceApi.get('USDCUSD'),
  ])
  loading.value = false
  balanceLoading.value = false

  if (gridsResult.status === 'fulfilled') groups.value = gridsResult.value
  if (statsResult.status === 'fulfilled') stats.value = statsResult.value
  if (balancesResult.status === 'fulfilled') balances.value = balancesResult.value
  if (priceResult.status === 'fulfilled') usdcusdPrice.value = priceResult.value.price
})
</script>
