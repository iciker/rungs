<template>
  <div>
    <n-h2>交易历史</n-h2>
    <n-spin :show="loading">
      <n-data-table
        :columns="columns"
        :data="trades"
        :pagination="{ pageSize: 20 }"
        :scroll-x="1100"
      />
    </n-spin>
  </div>
</template>

<script setup lang="ts">
import { ref, onMounted } from 'vue'
import { NH2, NDataTable, NSpin } from 'naive-ui'
import type { DataTableColumns } from 'naive-ui'
import { statsApi } from '@/api'
import type { Trade } from '@/api'

const trades = ref<Trade[]>([])
const loading = ref(false)

const columns: DataTableColumns<Trade> = [
  { title: 'ID', key: 'id', width: 70 },
  { title: 'Symbol', key: 'symbol', width: 110 },
  { title: '方向来源', key: 'source', width: 80 },
  { title: '买入价', key: 'buy_price', width: 110 },
  { title: '卖出价', key: 'sell_price', width: 110 },
  { title: '金额', key: 'amount', width: 110 },
  {
    title: '利润',
    key: 'profit',
    width: 110,
    render: (row) => {
      const v = parseFloat(row.profit ?? '0')
      return v >= 0 ? `+${row.profit}` : row.profit ?? '—'
    },
  },
  {
    title: '币安订单号',
    key: 'order_id',
    width: 130,
    ellipsis: { tooltip: true },
    render: (row) => row.order_id ?? '—',
  },
  {
    title: '成交时间',
    key: 'filled_at',
    width: 180,
    render: (row) => row.filled_at ? new Date(row.filled_at).toLocaleString('zh-CN') : '—',
  },
]

onMounted(async () => {
  loading.value = true
  try {
    trades.value = await statsApi.trades()
  } finally {
    loading.value = false
  }
})
</script>
