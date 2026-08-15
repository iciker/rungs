import { ref } from 'vue'
import { authApi } from '@/api'

const username = ref<string | null>(localStorage.getItem('username'))

async function login(user: string, pass: string) {
  const res = await authApi.login(user, pass)
  username.value = res.username
  localStorage.setItem('token', res.access_token)
  localStorage.setItem('username', res.username)
}

function logout() {
  username.value = null
  localStorage.removeItem('token')
  localStorage.removeItem('username')
}

const isLoggedIn = () => localStorage.getItem('token') !== null

export function useAuthStore() {
  return { username, login, logout, isLoggedIn }
}
