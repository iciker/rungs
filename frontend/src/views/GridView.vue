<template>
  <div>
    <div style="display: flex; justify-content: space-between; align-items: center; margin-bottom: 16px">
      <n-h2 style="margin: 0">{{ pageTitle }}</n-h2>
      <div style="display: flex; gap: 8px">
        <n-button v-if="isAutoPage" @click="openAutoCenter">智能建仓</n-button>
        <n-button v-if="isManualPage" type="primary" @click="openCreate">+ 新建网格</n-button>
      </div>
    </div>

    <n-spin :show="loading">
      <div v-for="group in filteredGroups" :key="group.name" style="margin-bottom: 32px">
        <!-- 分组标题行：symbol + 当前价 + 暂停开关 + 范围状态 -->
        <div style="display: flex; align-items: center; gap: 12px; margin-bottom: 12px; flex-wrap: wrap">
          <n-text strong style="font-size: 15px">{{ group.name }}</n-text>
          <n-tag v-if="prices[group.name]" type="info" size="small" :bordered="false">
            当前价：{{ prices[group.name] }}
          </n-tag>
          <n-switch
            :value="!isSymbolPaused(group)"
            :loading="pauseLoading[group.name]"
            @update:value="(val: boolean) => handlePauseToggle(group.name, !val)"
          >
            <template #checked>运行中</template>
            <template #unchecked>已暂停</template>
          </n-switch>
          <!-- 范围状态（仅智能建仓页显示）-->
          <template v-if="isAutoPage">
            <n-tag
              v-if="engineStatus[group.name]?.out_of_range"
              type="warning"
              size="small"
              :bordered="false"
            >
              价格超范围
            </n-tag>
            <n-tag v-else type="success" size="small" :bordered="false">
              价格在范围内
            </n-tag>
            <!-- 立即重建按钮 -->
            <n-button
              size="small"
              type="warning"
              :loading="recenterLoading[group.name]"
              @click="handleRecenterNow(group.name)"
            >
              立即重建
            </n-button>
          </template>
        </div>

        <n-data-table
          :columns="columns"
          :data="group.grid"
          :pagination="false"
        />
      </div>
      <n-empty v-if="!loading && filteredGroups.length === 0" description="暂无网格" />
    </n-spin>

    <!-- 智能建仓对话框 -->
    <n-modal v-model:show="autoCenterVisible" title="智能建仓" preset="card" style="width: 480px">
      <n-form ref="acFormRef" :model="acForm" :rules="acFormRules" label-placement="left" label-width="80">
        <n-form-item label="Symbol" path="symbol">
          <n-select
            v-model:value="acForm.symbol"
            filterable
            tag
            placeholder="选择或输入交易对，如 USDCUSD"
            :options="symbolOptions"
          />
        </n-form-item>
        <n-form-item label="格数" path="grid_count">
          <n-input-number v-model:value="acForm.grid_count" :min="1" :max="20" style="width: 100%" />
        </n-form-item>
        <n-form-item label="间距" path="spacing">
          <n-input v-model:value="acForm.spacing" placeholder="每格价格间距，如 0.0001" />
        </n-form-item>
        <n-alert type="info" :show-icon="false" style="margin-top: 8px">
          <n-text depth="3" style="font-size: 13px">
            将自动获取当前价格，按余额比例分配买卖格，并取消现有挂单后重新建仓。
          </n-text>
        </n-alert>
      </n-form>
      <template #footer>
        <div style="display: flex; justify-content: flex-end; gap: 8px">
          <n-button @click="autoCenterVisible = false">取消</n-button>
          <n-button type="primary" :loading="acSaving" @click="handleAutoCenter">确认建仓</n-button>
        </div>
      </template>
    </n-modal>

    <!-- 创建 / 编辑对话框 -->
    <n-modal v-model:show="dialogVisible" :title="dialogMode === 'create' ? '新建网格' : '编辑网格'" preset="card" style="width: 480px">
      <n-form ref="formRef" :model="form" :rules="formRules" label-placement="left" label-width="80">
        <n-form-item v-if="dialogMode === 'create'" label="Symbol" path="symbol">
          <n-select
            v-model:value="form.symbol"
            filterable
            tag
            placeholder="选择或输入交易对，如 USDCUSD"
            :options="symbolOptions"
          />
        </n-form-item>
        <n-form-item label="方向" path="side">
          <n-select v-model:value="form.side" :options="sideOptions" />
        </n-form-item>
        <n-form-item label="金额" path="amount">
          <n-input v-model:value="form.amount" placeholder="USDT 金额" />
        </n-form-item>
        <n-form-item label="买入价" path="buy_price">
          <n-input v-model:value="form.buy_price" placeholder="触发买入的价格" />
        </n-form-item>
        <n-form-item label="卖出价" path="sell_price">
          <n-input v-model:value="form.sell_price" placeholder="触发卖出的价格" />
        </n-form-item>
      </n-form>
      <template #footer>
        <div style="display: flex; justify-content: flex-end; gap: 8px">
          <n-button @click="dialogVisible = false">取消</n-button>
          <n-button type="primary" :loading="saving" @click="handleSave">保存</n-button>
        </div>
      </template>
    </n-modal>
  </div>
</template>

<script lang="ts">
// 模块级常量：所有组件实例共享，只分配一次
const symbolOptions = [
  { type: 'group', label: 'USD 稳定币', key: 'stable', children: [
    { label: 'USDCUSD', value: 'USDCUSD' },
    { label: 'USDCUSDT', value: 'USDCUSDT' },
    { label: 'BUSDUSDT', value: 'BUSDUSDT' },
  ]},
  { type: 'group', label: 'BTC 主流', key: 'btc', children: [
    { label: 'BTCUSDT', value: 'BTCUSDT' },
    { label: 'BTCUSD', value: 'BTCUSD' },
  ]},
  { type: 'group', label: 'ETH 主流', key: 'eth', children: [
    { label: 'ETHUSDT', value: 'ETHUSDT' },
    { label: 'ETHUSD', value: 'ETHUSD' },
  ]},
  { type: 'group', label: '其他', key: 'others', children: [
    { label: 'SOLUSDT', value: 'SOLUSDT' },
    { label: 'BNBUSDT', value: 'BNBUSDT' },
  ]},
]

const sideOptions = [
  { label: '买入 (buy)', value: 'buy' },
  { label: '卖出 (sell)', value: 'sell' },
]

</script>

<script setup lang="ts">
import { h, ref, computed, onMounted, onUnmounted, watch } from 'vue'
import { useRoute } from 'vue-router'
import {
  NH2, NButton, NDataTable, NSpin, NText, NEmpty, NModal, NForm, NFormItem,
  NInput, NInputNumber, NSelect, NAlert, NSwitch, NTag, useMessage,
} from 'naive-ui'
import type { DataTableColumns, FormInst, FormRules } from 'naive-ui'
import { gridApi, priceApi, engineApi } from '@/api'
import type { UserGrid, GridGroup, ReCenterStatus } from '@/api'

const route = useRoute()
const message = useMessage()

const groups = ref<GridGroup[]>([])
const prices = ref<Record<string, string>>({})
const loading = ref(false)
const saving = ref(false)
const pauseLoading = ref<Record<string, boolean>>({})
const toggleLoading = ref<Record<number, boolean>>({})

// ── 引擎状态（价格范围监控）────────────────────────────────────────────────
const engineStatus = ref<Record<string, ReCenterStatus>>({})
const recenterLoading = ref<Record<string, boolean>>({})
let statusPollTimer: ReturnType<typeof setInterval> | null = null

// ── 路由决定当前是哪个页面 ─────────────────────────────────────────────────────

const isAutoPage = computed(() => route.name === 'grids-auto')
const isManualPage = computed(() => route.name === 'grids-manual')
const pageTitle = computed(() => isAutoPage.value ? '智能建仓' : '普通建仓')

/** 按当前页面类型过滤 symbol 分组：只显示对应来源的网格 */
const filteredGroups = computed<GridGroup[]>(() => {
  const source = isAutoPage.value ? 'auto' : 'manual'
  return groups.value
    .map((g) => ({ ...g, grid: g.grid.filter((row) => row.source === source) }))
    .filter((g) => g.grid.length > 0)
})

// ── 表格列定义 ────────────────────────────────────────────────────────────────

const columns = computed<DataTableColumns<UserGrid>>(() => [
  { title: 'ID', key: 'id', width: 60 },
  { title: '方向', key: 'side', width: 70 },
  { title: '买入价', key: 'buy_price' },
  { title: '卖出价', key: 'sell_price' },
  { title: '金额', key: 'amount' },
  { title: '状态', key: 'status', width: 90 },
  { title: '订单号', key: 'order_id', ellipsis: true },
  {
    title: '挂单时间',
    key: 'updated_at',
    width: 120,
    render(row: UserGrid) {
      // updated_at 在 new→open（实际挂单到币安）时更新，反映真实建仓时间
      return new Date(row.updated_at).toLocaleString('zh-CN', {
        month: '2-digit',
        day: '2-digit',
        hour: '2-digit',
        minute: '2-digit',
      })
    },
  },
  {
    title: '开关',
    key: 'toggle',
    width: 80,
    render(row) {
      return h(NSwitch, {
        value: !row.is_paused,
        size: 'small',
        loading: toggleLoading.value[row.id] ?? false,
        'onUpdate:value': () => handleTogglePause(row),
      })
    },
  },
  {
    title: '操作',
    key: 'actions',
    width: 120,
    render(row) {
      return [
        h('a', { style: 'cursor:pointer; margin-right:12px', onClick: () => openEdit(row) }, '编辑'),
        h('a', { style: 'cursor:pointer; color:#d03050', onClick: () => handleDelete(row.id) }, '删除'),
      ]
    },
  },
])

// ── 数据加载 ──────────────────────────────────────────────────────────────────

function apiErrorMessage(e: unknown): string {
  const err = e as { response?: { data?: { message?: string } } }
  return err.response?.data?.message ?? '操作失败'
}

async function loadSymbolMap<T>(
  symbols: string[],
  load: (symbol: string) => Promise<T>,
): Promise<Record<string, T>> {
  const results = await Promise.allSettled(
    symbols.map((symbol) => load(symbol).then((value) => ({ symbol, value }))),
  )
  const values: Record<string, T> = {}
  for (const result of results) {
    if (result.status === 'fulfilled') {
      values[result.value.symbol] = result.value.value
    }
  }
  return values
}

async function loadData() {
  loading.value = true
  try {
    groups.value = await gridApi.list()
    // 并发加载各 symbol 的当前价格
    const symbols = groups.value.map((g) => g.name)
    prices.value = await loadSymbolMap(symbols, async (symbol) =>
      (await priceApi.get(symbol)).price,
    )
  } finally {
    loading.value = false
  }
}

/** 轮询所有 auto symbol 的引擎状态 */
async function pollEngineStatus() {
  if (!isAutoPage.value) return
  const symbols = groups.value.map((g) => g.name)
  engineStatus.value = await loadSymbolMap(symbols, engineApi.getStatus)
}

async function refresh() {
  await loadData()
  await pollEngineStatus()
}

/** 立即触发重新居中 */
async function handleRecenterNow(symbol: string) {
  recenterLoading.value[symbol] = true
  try {
    await engineApi.recenter(symbol)
    message.success(`${symbol} 已立即重新建仓`)
    await refresh()
  } catch (e: unknown) {
    message.error(apiErrorMessage(e))
  } finally {
    recenterLoading.value[symbol] = false
  }
}

onMounted(async () => {
  await refresh()
  // 每 30 秒轮询一次引擎状态
  statusPollTimer = setInterval(pollEngineStatus, 30_000)
})

onUnmounted(() => {
  if (statusPollTimer !== null) {
    clearInterval(statusPollTimer)
    statusPollTimer = null
  }
})

// 切换路由时重新加载（manual ↔ auto 切换）
watch(() => route.name, async () => {
  await refresh()
})

// ── 暂停 / 恢复（symbol 级别）────────────────────────────────────────────────

function isSymbolPaused(group: GridGroup): boolean {
  return group.grid.length > 0 && group.grid.every((g) => g.is_paused)
}

async function handlePauseToggle(symbol: string, pause: boolean) {
  pauseLoading.value[symbol] = true
  try {
    if (pause) {
      await gridApi.pause(symbol)
      message.success(`${symbol} 已暂停，Binance 挂单已撤销`)
    } else {
      await gridApi.resume(symbol)
      message.success(`${symbol} 已恢复并完成挂单`)
    }
    await loadData()
  } catch (e: unknown) {
    message.error(apiErrorMessage(e))
  } finally {
    pauseLoading.value[symbol] = false
  }
}

// ── 单格开关 ──────────────────────────────────────────────────────────────────

async function handleTogglePause(row: UserGrid) {
  toggleLoading.value[row.id] = true
  try {
    const result = await gridApi.togglePause(row.id)
    const action = result.is_paused ? '已暂停' : '已恢复'
    message.success(`订单 #${row.id} ${action}`)
    await loadData()
  } catch (e: unknown) {
    message.error(apiErrorMessage(e))
  } finally {
    delete toggleLoading.value[row.id]
  }
}

// ── 智能建仓 ──────────────────────────────────────────────────────────────────

const autoCenterVisible = ref(false)
const acSaving = ref(false)
const acFormRef = ref<FormInst | null>(null)
const acForm = ref({ symbol: 'USDCUSD', grid_count: 4, spacing: '0.0001' })

const acFormRules: FormRules = {
  symbol: [{ required: true, message: '请填写 Symbol', trigger: 'blur' }],
  grid_count: [{ required: true, type: 'number', message: '请填写格数', trigger: 'blur' }],
  spacing: [{ required: true, message: '请填写间距', trigger: 'blur' }],
}

function openAutoCenter() {
  autoCenterVisible.value = true
}

async function handleAutoCenter() {
  try {
    await acFormRef.value?.validate()
  } catch {
    return
  }
  acSaving.value = true
  try {
    const result = await gridApi.autoCenter({
      symbol: acForm.value.symbol,
      grid_count: acForm.value.grid_count,
      spacing: acForm.value.spacing,
    })
    message.success(`建仓成功：卖格 ${result.sell_count} 个（每格 ${result.sell_amount}），买格 ${result.buy_count} 个（每格 ${result.buy_amount}），当前价 ${result.current_price}`)
    autoCenterVisible.value = false
    await refresh()
  } catch (e: unknown) {
    message.error(apiErrorMessage(e))
  } finally {
    acSaving.value = false
  }
}

// ── 创建 / 编辑对话框 ─────────────────────────────────────────────────────────

const dialogVisible = ref(false)
const dialogMode = ref<'create' | 'edit'>('create')
const editingId = ref<number | null>(null)
const formRef = ref<FormInst | null>(null)

const emptyForm = () => ({ symbol: '', side: 'buy', amount: '', buy_price: '', sell_price: '' })
const form = ref(emptyForm())

const formRules: FormRules = {
  symbol: [{ required: true, message: '请填写 Symbol', trigger: 'blur' }],
  amount: [{ required: true, message: '请填写金额', trigger: 'blur' }],
  buy_price: [{ required: true, message: '请填写买入价', trigger: 'blur' }],
  sell_price: [{ required: true, message: '请填写卖出价', trigger: 'blur' }],
}

function openCreate() {
  dialogMode.value = 'create'
  form.value = emptyForm()
  editingId.value = null
  dialogVisible.value = true
}

function openEdit(row: UserGrid) {
  dialogMode.value = 'edit'
  editingId.value = row.id
  form.value = {
    symbol: row.symbol,
    side: row.side,
    amount: row.amount,
    buy_price: row.buy_price,
    sell_price: row.sell_price,
  }
  dialogVisible.value = true
}

async function handleSave() {
  try {
    await formRef.value?.validate()
  } catch {
    return
  }
  saving.value = true
  try {
    if (dialogMode.value === 'create') {
      await gridApi.create({
        symbol: form.value.symbol,
        amount: form.value.amount,
        buy_price: form.value.buy_price,
        sell_price: form.value.sell_price,
        side: form.value.side,
      })
      message.success('网格创建成功')
    } else {
      if (editingId.value === null) return
      await gridApi.update(editingId.value, {
        amount: form.value.amount,
        buy_price: form.value.buy_price,
        sell_price: form.value.sell_price,
        side: form.value.side,
      })
      message.success('网格更新成功')
    }
    dialogVisible.value = false
    await loadData()
  } catch (e: unknown) {
    message.error(apiErrorMessage(e))
  } finally {
    saving.value = false
  }
}

async function handleDelete(id: number) {
  try {
    await gridApi.delete(id)
    message.success('删除成功')
    await loadData()
  } catch {
    message.error('删除失败')
  }
}
</script>
