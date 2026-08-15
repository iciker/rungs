import { createRouter, createWebHistory } from 'vue-router'
import { useAuthStore } from '@/stores/auth'

const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: '/login',
      name: 'login',
      component: () => import('@/views/LoginView.vue'),
    },
    {
      path: '/',
      component: () => import('@/layouts/MainLayout.vue'),
      meta: { requiresAuth: true },
      children: [
        {
          path: '',
          name: 'dashboard',
          component: () => import('@/views/DashboardView.vue'),
        },
        {
          path: 'grids/manual',
          name: 'grids-manual',
          component: () => import('@/views/GridView.vue'),
        },
        {
          path: 'grids/auto',
          name: 'grids-auto',
          component: () => import('@/views/GridView.vue'),
        },
        {
          path: 'trades',
          name: 'trades',
          component: () => import('@/views/TradesView.vue'),
        },
      ],
    },
  ],
})

router.beforeEach((to) => {
  const auth = useAuthStore()
  if (to.meta.requiresAuth && !auth.isLoggedIn()) {
    return { name: 'login' }
  }
})

export default router
