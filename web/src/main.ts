import { createApp } from 'vue'
import { createPinia } from 'pinia'
import App from './App.vue'
import router from './router'
import { useAuthStore } from './stores/auth'
import './assets/main.css'

const app = createApp(App)
app.use(createPinia())

// 先恢复会话再挂路由：守卫能拿到正确的登录态
const auth = useAuthStore()
void auth.restore().finally(() => {
  app.use(router)
  app.mount('#app')
})
