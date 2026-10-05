import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { createApp } from "vue";
import App from "./App.vue";
import ExplorerApp from "./explorer/ExplorerApp.vue";
import "./style.css";

const isExplorer = getCurrentWebviewWindow().label === "explorer";
createApp(isExplorer ? ExplorerApp : App).mount("#app");
