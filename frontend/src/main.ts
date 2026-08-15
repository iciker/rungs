import { createApp } from 'vue'
import router from './router'
import App from './App.vue'

// Naive UI 字体（可选，提升渲染质量）
import 'vfonts/Lato.css'
import 'vfonts/FiraCode.css'

const app = createApp(App)
app.use(router)
app.mount('#app')
