import axios from 'axios'
import { storage } from '@/lib/storage'

// `/admin` 与 `/grok/admin` 复用同一套界面；根据当前挂载路径选择对应的
// 管理 API，避免 Grok 面板误操作 Kiro 凭据池。
const isGrokAdmin = window.location.pathname === '/grok/admin'
  || window.location.pathname.startsWith('/grok/admin/')

// 创建 axios 实例
export const api = axios.create({
  baseURL: isGrokAdmin ? '/grok/api/admin' : '/api/admin',
  headers: {
    'Content-Type': 'application/json',
  },
})

// 请求拦截器添加 API Key
api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) {
    config.headers['x-api-key'] = apiKey
  }
  return config
})
