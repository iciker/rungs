<template>
  <n-layout style="height: 100vh">
    <n-layout-header bordered style="height: 56px; padding: 0 24px; display: flex; align-items: center; justify-content: space-between">
      <div style="display: flex; align-items: center; gap: 12px">
        <n-text strong style="font-size: 18px">Binance Grid Bot</n-text>
      </div>
      <div style="display: flex; align-items: center; gap: 16px">
        <n-text depth="3">{{ auth.username }}</n-text>
        <n-button text @click="handleLogout">退出</n-button>
      </div>
    </n-layout-header>

    <n-layout has-sider style="height: calc(100vh - 56px)">
      <n-layout-sider bordered width="180" content-style="padding: 12px 0">
        <n-menu :options="menuOptions" :value="activeKey" @update:value="navigate" />
      </n-layout-sider>

      <n-layout-content content-style="padding: 24px; overflow-y: auto">
        <router-view />
      </n-layout-content>
    </n-layout>
  </n-layout>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { useRouter, useRoute } from 'vue-router'
import { NLayout, NLayoutHeader, NLayoutSider, NLayoutContent, NMenu, NButton, NText } from 'naive-ui'
import type { MenuOption } from 'naive-ui'
import { useAuthStore } from '@/stores/auth'

const auth = useAuthStore()
const router = useRouter()
const route = useRoute()

const menuOptions: MenuOption[] = [
  { label: '仪表盘', key: 'dashboard' },
  { label: '普通建仓', key: 'grids-manual' },
  { label: '智能建仓', key: 'grids-auto' },
  { label: '交易历史', key: 'trades' },
]

const activeKey = computed(() => route.name as string)

function navigate(key: string) {
  router.push({ name: key })
}

function handleLogout() {
  auth.logout()
  router.push({ name: 'login' })
}
</script>
