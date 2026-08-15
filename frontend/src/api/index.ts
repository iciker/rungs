import axios from 'axios'

const http = axios.create({ baseURL: '/api' })

// 自动带上 JWT
http.interceptors.request.use((config) => {
  const token = localStorage.getItem('token')
  if (token) config.headers.Authorization = `Bearer ${token}`
  return config
})

// 统一提取 data 字段，401 自动跳转登录
http.interceptors.response.use(
  (res) => res,
  (err) => {
    // 只在「本来就带着 token、结果被判过期」时才踢回登录页。
    // 登录接口自己返回的 401 是"密码错了"，若也硬跳转，页面会整个重载，
    // 用户永远看不到错误提示，只会觉得点了没反应。
    const isLoginAttempt = err.config?.url?.includes('/auth/login')
    if (err.response?.status === 401 && !isLoginAttempt && localStorage.getItem('token')) {
      localStorage.removeItem('token')
      localStorage.removeItem('username')
      window.location.href = '/login'
    }
    return Promise.reject(err)
  },
)

// ── 类型定义 ──────────────────────────────────────────────────────────────────

export interface LoginResponse {
  access_token: string
  username: string
}

export interface UserGrid {
  id: number
  user_id: number
  order_id: string | null
  amount: string
  buy_price: string
  sell_price: string
  side: string
  symbol: string
  status: string
  source: string  // 'manual' | 'auto'
  is_paused: boolean
  version: number
  created_at: string
  updated_at: string
}

export interface GridGroup {
  name: string
  grid: UserGrid[]
}

export interface Trade {
  id: number
  user_id: number
  symbol: string
  order_id: string | null
  amount: string
  buy_price: string
  sell_price: string
  profit: string | null
  source: string | null
  source_id: number | null
  filled_at: string | null
}

export interface ProfitStats {
  total_profit: string
  trade_count: number
}

export interface GridValues {
  amount: string
  buy_price: string
  sell_price: string
  side: string
}

export interface CreateGridRequest extends GridValues {
  symbol: string
}

export type UpdateGridRequest = GridValues

export interface AutoCenterRequest {
  symbol: string
  grid_count: number
  spacing: string
}

export interface AutoCenterResponse {
  ids: number[]
  sell_count: number
  buy_count: number
  current_price: string
  sell_amount: string
  buy_amount: string
}

const responseData = <T>(res: { data: T }): T => res.data

// ── 认证 ──────────────────────────────────────────────────────────────────────

export const authApi = {
  login: (username: string, password: string) =>
    http.post<LoginResponse>('/auth/login', { username, password }).then(responseData),
}

// ── 网格 ──────────────────────────────────────────────────────────────────────

export const gridApi = {
  list: () =>
    http.get<GridGroup[]>('/user/grids').then(responseData),

  create: (body: CreateGridRequest) =>
    http.post<{ id: number }>('/user/grids', body).then(responseData),

  update: (id: number, body: UpdateGridRequest) =>
    http.put<null>(`/user/grids/${id}`, body).then(responseData),

  delete: (id: number) =>
    http.delete<null>(`/user/grids/${id}`).then(responseData),

  autoCenter: (body: AutoCenterRequest) =>
    http.post<AutoCenterResponse>('/user/grids/auto-center', body).then(responseData),

  pause: (symbol: string) =>
    http.post<{ paused: number }>('/user/grids/pause', { symbol }).then(responseData),

  resume: (symbol: string) =>
    http.post<{ resumed: number }>('/user/grids/resume', { symbol }).then(responseData),

  togglePause: (id: number) =>
    http.post<{ is_paused: boolean }>(`/user/grids/${id}/toggle-pause`).then(responseData),
}

// ── 引擎状态 ──────────────────────────────────────────────────────────────────

export interface ReCenterStatus {
  out_of_range: boolean
  minutes_out: number
}

export const engineApi = {
  /** 查询某 symbol 价格是否超出网格范围 */
  getStatus: (symbol: string) =>
    http.get<ReCenterStatus>(`/user/engine-status/${symbol}`).then(responseData),

  /** 立即触发重新居中（手动重建） */
  recenter: (symbol: string) =>
    http.post<{ symbol: string; status: string }>(`/user/recenter/${symbol}`).then(responseData),
}

// ── 价格 ──────────────────────────────────────────────────────────────────────

export const priceApi = {
  get: (symbol: string) =>
    http.get<{ symbol: string; price: string }>(`/user/price/${symbol}`).then(responseData),
}

// ── Binance 余额 ──────────────────────────────────────────────────────────────

export interface SpotBalance {
  asset: string
  free: string
  locked: string
}

export const balanceApi = {
  spot: () =>
    http.get<SpotBalance[]>('/user/balance').then(responseData),
}

// ── 收益 / 交易历史 ───────────────────────────────────────────────────────────

export const statsApi = {
  profitStats: () =>
    http.get<ProfitStats>('/user/profit-stats').then(responseData),

  trades: () =>
    http.get<Trade[]>('/user/trades').then(responseData),
}
