// 路由：登录页 / 聊天页，守卫按登录态分流。
import { createRouter, createWebHistory } from 'vue-router'
import { useAuthStore } from '@/stores/auth'

const router = createRouter({
  history: createWebHistory(),
  routes: [
    { path: '/', redirect: '/chat' },
    {
      path: '/login',
      name: 'login',
      component: () => import('@/views/LoginView.vue'),
    },
    {
      path: '/chat',
      name: 'chat',
      component: () => import('@/views/ChatView.vue'),
    },
    {
      path: '/friends',
      name: 'friends',
      component: () => import('@/views/FriendsView.vue'),
    },
    {
      path: '/groups',
      name: 'groups',
      component: () => import('@/views/GroupsView.vue'),
    },
    {
      // 会议页（阶段 9）：路径参数即群 ID——一群一间常驻会议室
      path: '/meeting/:groupId',
      name: 'meeting',
      component: () => import('@/views/MeetingView.vue'),
    },
  ],
})

// 守卫：未登录一律去 /login（含刷新页面时——auth.restore 在 main.ts 先行）
router.beforeEach((to) => {
  const auth = useAuthStore()
  if (!auth.isLoggedIn && to.name !== 'login') return { name: 'login' }
  if (auth.isLoggedIn && to.name === 'login') return { name: 'chat' }
})

export default router
